//! Expression-level tinct formatters — SCN serializers for structured values.
//!
//! These functions produce canonical tinct source text for Self-Contained Normal Form
//! (SCN) serialization. They are the inverse of the parser: where the parser produces
//! AST nodes from source text, these functions format values back to tinct source text.
//!
//! Co-location with the grammar (eventual convergence with src/parser.rs) ensures the
//! parse↔format pair for each expression form is visible together.

use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::ast::{CoreExpr, Param, Spanned};
use crate::env::Env;
use crate::eval::core_expr_is_static_key;
use crate::eval::EvalContext;
use crate::lexer::{fmt_float, fmt_int, fmt_string};
use crate::value::{HashableValue, Thunk, Value};

/// Format a dict as a tinct literal `[k: v  ...]`.
///
/// Keys are formatted as bare identifiers when possible, otherwise as strings or integers.
/// Values are recursively formatted via `to_tinct`.
///
/// `ctx` is `None` when the caller guarantees no Function values are present (e.g., profiling
/// path serializing scalar dicts). Function serialization requires `Some(ctx)` for stdlib
/// membership checks.
pub fn fmt_dict(
    map: &IndexMap<HashableValue, Arc<crate::value::Thunk>>,
    ctx: Option<&Arc<EvalContext>>,
) -> Result<String, String> {
    let mut out = String::from("[");
    for (i, (key, thunk)) in map.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }

        // Format key
        match key {
            HashableValue::Int(n) => {
                out.push_str(&fmt_int(*n));
                out.push(':');
            }
            HashableValue::Str(s) => {
                // Use bare identifier syntax when the string is a valid identifier
                if is_valid_identifier(s) {
                    out.push_str(s);
                } else {
                    out.push_str(&fmt_string(s));
                }
                out.push(':');
            }
            HashableValue::Bool(b) => {
                if *b {
                    out.push_str("Boolean.True:");
                } else {
                    out.push_str("Boolean.False:");
                }
            }
            HashableValue::Variant { tag, payload } => match payload {
                None => {
                    out.push_str(tag);
                    out.push(':');
                }
                Some(p) => {
                    out.push('[');
                    out.push_str(tag);
                    out.push(' ');
                    out.push_str(&p.to_string());
                    out.push_str("]:");
                }
            },
            HashableValue::Dict(pairs) => {
                out.push('[');
                for (j, (k, v)) in pairs.iter().enumerate() {
                    if j > 0 {
                        out.push_str("  ");
                    }
                    out.push_str(&k.to_string());
                    out.push_str(": ");
                    out.push_str(&v.to_string());
                }
                out.push_str("]:");
            }
        }

        // Format value — retrieve the materialized value and recursively serialize.
        // ctx is passed to to_tinct below; it is required for Function serialization.
        let value = thunk
            .try_get_value()
            .cloned()
            .ok_or("dict value not materialized")?;
        out.push(' ');
        // ctx is passed to to_tinct; only required if value is a Function.
        out.push_str(&value.to_tinct(ctx)?);
    }
    out.push(']');
    Ok(out)
}

/// Format a variant as a tinct literal.
///
/// Nullary constructors: `Tag`
/// Unary constructors: `[Tag payload]`
pub fn fmt_variant(
    tag: &str,
    payload: Option<Arc<Thunk>>,
    ctx: Option<&Arc<EvalContext>>,
) -> Result<String, String> {
    match payload {
        None => {
            // Nullary constructor — just the tag
            Ok(tag.to_string())
        }
        Some(thunk) => {
            // Unary constructor — [Tag payload]
            let value = thunk
                .try_get_value()
                .cloned()
                .ok_or("variant payload not materialized")?;
            Ok(format!("[{} {}]", tag, value.to_tinct(ctx)?))
        }
    }
}

/// Format a function as a tinct literal `[fn [let params] body]` with closure substitution.
///
/// Implements the SCN algorithm for functions (§Functions in data-streaming.md):
/// 1. Identify non-stdlib free variables in the body
/// 2. Substitute captured bindings with their SCN values
/// 3. Apply capture-avoiding alpha-renaming when necessary
/// 4. Serialize the resulting expression
pub fn fmt_fn(
    params: &[Param],
    body: &Arc<Spanned<CoreExpr>>,
    env: &Env,
    ctx: &Arc<EvalContext>,
) -> Result<String, String> {
    // Build initial param scope from the top-level parameters.
    let param_scope: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();

    // Step 1: Collect all free (non-stdlib, non-param-bound) variable names.
    // Build a minimal stdlib name set from the FlatEnv root (env_id=0) slot names.
    let stdlib_name_set: HashSet<String> = crate::builtins_core::core_builtins()
        .iter()
        .map(|def| def.name.to_string())
        .collect();
    // Build a temporary Env with those names so collect_free_vars can check them.
    let stdlib_env_inner = {
        let mut e = crate::env::Env::new();
        for name in &stdlib_name_set {
            e.insert_slot_name_only(name.clone());
        }
        e
    };
    let stdlib_env = std::sync::RwLock::new(stdlib_env_inner);
    let stdlib_env = stdlib_env
        .read()
        .map_err(|_| "failed to read stdlib_env lock")?;
    let mut free_vars: HashSet<String> = HashSet::new();
    collect_free_vars(&body.node, &param_scope, &stdlib_env, &mut free_vars);

    // Step 2: Build substitution map: name → SCN string.
    // For each captured name, look it up in env and serialize its value.
    // Env is type-metadata only — no runtime values available for substitution.
    // Substitution requires FlatEnv closure value access (B-515 tracks arm binding allocation).
    // Leave all free vars as-is (ambient names resolved through the enclosing scope chain).
    let substitutions: HashMap<String, String> = HashMap::new();
    let _ = env; // env parameter retained for future FlatEnv wiring (B-515)

    // Step 3: Capture-avoiding alpha-rename.
    // For each top-level param p, check if p appears as a free-standing identifier
    // in any substitution value. If so, rename p to a fresh gensym name throughout
    // params and body.
    //
    // We extract identifiers structurally (tokenizing, skipping string literals)
    // rather than using substring containment, which would produce false positives
    // (e.g., param "x" matching inside substitution value "x-coordinate").
    let substitution_text: String = substitutions
        .values()
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let sub_identifiers = extract_identifiers(&substitution_text);
    let mut rename_map: HashMap<String, String> = HashMap::new();
    for param in params {
        if sub_identifiers.contains(param.name.as_str()) {
            // Scope '𝒻' (U+1D4BB, script f) marks names from formatter capture-avoiding renaming.
            let fresh = crate::builtins_meta::gensym_fresh('𝒻', &param.name);
            rename_map.insert(param.name.clone(), fresh);
        }
    }

    // Step 4: Serialize the body with substitutions and renames applied.
    let body_str = core_expr_to_tinct(&body.node, &param_scope, &substitutions, &rename_map, ctx)?;

    // Serialize the (possibly renamed) parameter list.
    let params_str: Vec<String> = params
        .iter()
        .map(|p| {
            let name = rename_map
                .get(&p.name)
                .cloned()
                .unwrap_or_else(|| p.name.clone());
            if p.variadic {
                format!("...{}", name)
            } else {
                name
            }
        })
        .collect();

    Ok(format!("[fn [let {}] {}]", params_str.join(" "), body_str))
}

// ────────────────────────────────────────────────────────────────────────────────
// Identifier extraction for capture-avoidance
// ────────────────────────────────────────────────────────────────────────────────

/// Characters that delimit identifiers in serialized tinct text.
/// Mirrors the denylist in `Lexer::is_var_ident_char` (src/lexer.rs).
fn is_ident_delimiter(c: char) -> bool {
    matches!(
        c,
        ' ' | '\t' | '\r' | '\n' | '[' | ']' | ':' | ';' | '#' | '"' | '@' | '.' | '|'
    )
}

/// Extract the set of identifier tokens from serialized tinct text.
///
/// This performs structural tokenization rather than substring matching,
/// correctly skipping content inside string literals. An identifier is any
/// maximal sequence of non-delimiter characters that appears outside of
/// a quoted string.
///
/// Used by capture-avoidance to determine whether a parameter name appears
/// as a free-standing identifier in a substitution value, not merely as a
/// substring of some longer token or inside a string literal.
fn extract_identifiers(text: &str) -> HashSet<&str> {
    let mut identifiers = HashSet::new();
    let mut chars = text.char_indices().peekable();

    while let Some(&(i, c)) = chars.peek() {
        if c == '"' {
            // Skip over string literal content (not identifiers).
            chars.next(); // consume opening quote
            loop {
                match chars.next() {
                    Some((_, '\\')) => {
                        // Skip escaped character (e.g., \", \\, \n)
                        chars.next();
                    }
                    Some((_, '"')) => break, // closing quote
                    None => break,           // unterminated string — stop
                    _ => {}                  // ordinary string character
                }
            }
        } else if is_ident_delimiter(c) {
            // Skip delimiter
            chars.next();
        } else {
            // Start of an identifier token — accumulate non-delimiter chars
            let start = i;
            let mut end = i;
            while let Some(&(j, c2)) = chars.peek() {
                if is_ident_delimiter(c2) {
                    break;
                }
                end = j;
                chars.next();
            }
            // end is the byte index of the last char; we need the byte after it
            let ident = &text[start..end + text[end..].chars().next().map_or(0, |c| c.len_utf8())];
            if !ident.is_empty() {
                identifiers.insert(ident);
            }
        }
    }

    identifiers
}

// ────────────────────────────────────────────────────────────────────────────────
// CoreExpr traversal helpers
// ────────────────────────────────────────────────────────────────────────────────

/// Walk a `CoreExpr` and collect names that are free references to user-env bindings:
/// not in `param_scope` and not in `stdlib_env`.
///
/// `param_scope` grows as we descend into `Fn` bodies and `Match` arms.
fn collect_free_vars(
    expr: &CoreExpr,
    param_scope: &HashSet<String>,
    stdlib_env: &Env,
    out: &mut HashSet<String>,
) {
    match expr {
        // Leaves — no variable references
        CoreExpr::Int(_)
        | CoreExpr::U64(_)
        | CoreExpr::Float(_)
        | CoreExpr::Str(_)
        | CoreExpr::Placeholder
        | CoreExpr::Rest(_)
        | CoreExpr::UnitVariant { .. } => {}

        // Variant: tag is a literal, payload may contain variable references.
        CoreExpr::Variant { payload, .. } => {
            if let Some(inner) = payload {
                collect_free_vars(&inner.node, param_scope, stdlib_env, out);
            }
        }

        // Variable references — the decision point
        CoreExpr::Var { name, .. } => {
            if !param_scope.contains(name.as_str())
                && stdlib_env.get_scheme(name.as_str()).is_none()
            {
                out.insert(name.clone());
            }
        }

        // Annotated Var (Var { annotation: Some(_) }) is handled by the Var arm above.
        CoreExpr::Sequential(exprs) => {
            // Mirror the resolver's sequential scope injection (resolve.rs:118-134):
            // after each intermediate expression, collect any static dict keys it introduces
            // and add them to param_scope for subsequent expressions.
            let mut seq_scope = param_scope.clone();
            for (i, e) in exprs.iter().enumerate() {
                collect_free_vars(&e.node, &seq_scope, stdlib_env, out);
                // After all but the last expression, inject static dict keys into scope.
                if i + 1 < exprs.len() {
                    if let CoreExpr::Dict(entries) = &e.node {
                        for entry in entries {
                            if let Some(key) = &entry.node.key {
                                if core_expr_is_static_key(&key.node) {
                                    if let CoreExpr::Str(name) = &key.node {
                                        seq_scope.insert(name.clone());
                                    // Annotated Var (Var { annotation: Some(_) }) is a static key.
                                    } else if let CoreExpr::Var {
                                        name,
                                        annotation: Some(_),
                                        ..
                                    } = &key.node
                                    {
                                        seq_scope.insert(name.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        CoreExpr::Dict(entries) => {
            // Mirror the resolver's dict letrec scoping (resolve.rs:83-97, eval_dict.rs:66-68):
            // all static key names are bound within the dict scope and visible to all entry
            // values (letrec, enabling mutual recursion). Collect static keys first, then
            // extend param_scope before recursing into values so we do not treat dict-local
            // variable references as free.
            let mut dict_scope = param_scope.clone();
            for entry in entries {
                if let Some(key) = &entry.node.key {
                    if core_expr_is_static_key(&key.node) {
                        if let CoreExpr::Str(name) = &key.node {
                            dict_scope.insert(name.clone());
                        // Annotated Var (Var { annotation: Some(_) }) is a static key.
                        } else if let CoreExpr::Var {
                            name,
                            annotation: Some(_),
                            ..
                        } = &key.node
                        {
                            dict_scope.insert(name.clone());
                        }
                    }
                }
            }
            // Keys are evaluated in the parent scope (key isolation invariant — keys must not
            // see sibling bindings). Values are evaluated in the letrec dict_scope.
            for entry in entries {
                if let Some(key) = &entry.node.key {
                    collect_free_vars(&key.node, param_scope, stdlib_env, out);
                }
                collect_free_vars(&entry.node.value.node, &dict_scope, stdlib_env, out);
            }
        }

        CoreExpr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            collect_free_vars(&func.node, param_scope, stdlib_env, out);
            for arg in args {
                collect_free_vars(&arg.node, param_scope, stdlib_env, out);
            }
            for named_arg in named_args {
                collect_free_vars(&named_arg.node.value.node, param_scope, stdlib_env, out);
            }
        }

        CoreExpr::Fn { params, body, .. } => {
            // Extend param scope with this lambda's params before recursing into body.
            let mut inner_scope = param_scope.clone();
            for p in params {
                inner_scope.insert(p.node.name.clone());
            }
            collect_free_vars(&body.node, &inner_scope, stdlib_env, out);
        }

        CoreExpr::TypeAssert { expr, .. } => {
            collect_free_vars(&expr.node, param_scope, stdlib_env, out);
        }

        CoreExpr::Match { scrutinee, arms } => {
            collect_free_vars(&scrutinee.node, param_scope, stdlib_env, out);
            for arm in arms {
                // T-1750: patterns are now SurfaceNode and introduce no bindings
                // (only [case [let ...]] introduces bindings)
                if let Some(guard) = &arm.guard {
                    collect_free_vars(&guard.node, param_scope, stdlib_env, out);
                }
                collect_free_vars(&arm.body.node, param_scope, stdlib_env, out);
            }
        }

        // Quote is opaque AST data — do not substitute into it.
        // Only recurse into nested Unquote/UnquoteSplice sub-expressions.
        CoreExpr::Quote(inner) => {
            collect_free_vars_in_quote(&inner.node, 1, param_scope, stdlib_env, out);
        }

        CoreExpr::Unquote(inner) | CoreExpr::UnquoteSplice(inner) => {
            // Unquote outside Quote is a no-op in the surface language; recurse normally.
            collect_free_vars(&inner.node, param_scope, stdlib_env, out);
        }

        CoreExpr::PatternDecl { bindings } | CoreExpr::LetDecl { bindings } => {
            for binding in bindings {
                collect_free_vars(&binding.node, param_scope, stdlib_env, out);
            }
        }

        CoreExpr::CaseArm {
            let_bindings,
            pattern,
            body,
        } => {
            // Extract binding names from let_bindings to build the arm scope.
            let mut arm_scope = param_scope.clone();
            if let CoreExpr::LetDecl { bindings } = &let_bindings.node {
                for binding in bindings {
                    if let CoreExpr::Str(name) = &binding.node {
                        arm_scope.insert(name.clone());
                    // Annotated Var (Var { annotation: Some(_) }) is also handled by Var arm.
                    } else if let CoreExpr::Var { name, .. } = &binding.node {
                        arm_scope.insert(name.clone());
                    }
                }
            }

            collect_free_vars(&let_bindings.node, param_scope, stdlib_env, out);
            collect_free_vars(&pattern.node, param_scope, stdlib_env, out);
            collect_free_vars(&body.node, &arm_scope, stdlib_env, out);
        }
    }
}

/// Recurse into a quoted expression, tracking quote depth.
/// At depth > 0, Unquote/UnquoteSplice decrements depth and resumes normal traversal.
fn collect_free_vars_in_quote(
    expr: &CoreExpr,
    depth: usize,
    param_scope: &HashSet<String>,
    stdlib_env: &Env,
    out: &mut HashSet<String>,
) {
    match expr {
        CoreExpr::Quote(inner) => {
            collect_free_vars_in_quote(&inner.node, depth + 1, param_scope, stdlib_env, out);
        }
        CoreExpr::Unquote(inner) | CoreExpr::UnquoteSplice(inner) => {
            if depth == 1 {
                // Returning to evaluation context — recurse normally
                collect_free_vars(&inner.node, param_scope, stdlib_env, out);
            } else {
                collect_free_vars_in_quote(&inner.node, depth - 1, param_scope, stdlib_env, out);
            }
        }
        // All other nodes inside Quote: don't substitute (opaque AST), but recurse
        // to find any nested Unquote sub-expressions.
        CoreExpr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            collect_free_vars_in_quote(&func.node, depth, param_scope, stdlib_env, out);
            for arg in args {
                collect_free_vars_in_quote(&arg.node, depth, param_scope, stdlib_env, out);
            }
            for named_arg in named_args {
                collect_free_vars_in_quote(
                    &named_arg.node.value.node,
                    depth,
                    param_scope,
                    stdlib_env,
                    out,
                );
            }
        }
        CoreExpr::Fn { body, .. } => {
            collect_free_vars_in_quote(&body.node, depth, param_scope, stdlib_env, out);
        }
        CoreExpr::Dict(entries) => {
            for entry in entries {
                if let Some(key) = &entry.node.key {
                    collect_free_vars_in_quote(&key.node, depth, param_scope, stdlib_env, out);
                }
                collect_free_vars_in_quote(
                    &entry.node.value.node,
                    depth,
                    param_scope,
                    stdlib_env,
                    out,
                );
            }
        }
        CoreExpr::Sequential(exprs) => {
            for e in exprs {
                collect_free_vars_in_quote(&e.node, depth, param_scope, stdlib_env, out);
            }
        }
        CoreExpr::Match { scrutinee, arms } => {
            collect_free_vars_in_quote(&scrutinee.node, depth, param_scope, stdlib_env, out);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_free_vars_in_quote(&guard.node, depth, param_scope, stdlib_env, out);
                }
                collect_free_vars_in_quote(&arm.body.node, depth, param_scope, stdlib_env, out);
            }
        }
        CoreExpr::TypeAssert { expr, .. } => {
            collect_free_vars_in_quote(&expr.node, depth, param_scope, stdlib_env, out);
        }
        CoreExpr::CaseArm {
            let_bindings,
            pattern,
            body,
        } => {
            // Extract binding names from let_bindings to build the arm scope.
            let mut arm_scope = param_scope.clone();
            if let CoreExpr::LetDecl { bindings } = &let_bindings.node {
                for binding in bindings {
                    if let CoreExpr::Str(name) = &binding.node {
                        arm_scope.insert(name.clone());
                    // Annotated Var (Var { annotation: Some(_) }) is also handled by Var arm.
                    } else if let CoreExpr::Var { name, .. } = &binding.node {
                        arm_scope.insert(name.clone());
                    }
                }
            }

            collect_free_vars_in_quote(&let_bindings.node, depth, param_scope, stdlib_env, out);
            collect_free_vars_in_quote(&pattern.node, depth, param_scope, stdlib_env, out);
            collect_free_vars_in_quote(&body.node, depth, &arm_scope, stdlib_env, out);
        }
        CoreExpr::PatternDecl { bindings } | CoreExpr::LetDecl { bindings } => {
            for b in bindings {
                collect_free_vars_in_quote(&b.node, depth, param_scope, stdlib_env, out);
            }
        }
        // Variant: tag is a literal, payload may contain variable references.
        CoreExpr::Variant { payload, .. } => {
            if let Some(inner) = payload {
                collect_free_vars_in_quote(&inner.node, depth, param_scope, stdlib_env, out);
            }
        }
        // Leaves — nothing to do even inside quotes
        CoreExpr::Int(_)
        | CoreExpr::U64(_)
        | CoreExpr::Float(_)
        | CoreExpr::Str(_)
        | CoreExpr::Var { .. }  // includes annotated Var (Var { annotation: Some(_) })
        | CoreExpr::Rest(_)
        | CoreExpr::UnitVariant { .. }
        | CoreExpr::Placeholder => {}
    }
}

/// Convert a `CoreExpr` to tinct source text, applying substitutions and renames.
///
/// - `param_scope`: names bound by enclosing `Fn` params or match arm patterns (leave as-is)
/// - `substitutions`: map from captured variable name to its pre-computed SCN string
/// - `rename_map`: map from original param name to alpha-renamed gensym name
/// - `ctx`: evaluation context (for nested function serialization)
fn core_expr_to_tinct(
    expr: &CoreExpr,
    param_scope: &HashSet<String>,
    substitutions: &HashMap<String, String>,
    rename_map: &HashMap<String, String>,
    ctx: &Arc<EvalContext>,
) -> Result<String, String> {
    match expr {
        // Literals — direct serialization
        CoreExpr::Int(n) => Ok(fmt_int(*n)),
        CoreExpr::U64(n) => Ok(format!("{n}u")),
        CoreExpr::Float(f) => fmt_float(*f),
        CoreExpr::Str(s) => Ok(fmt_string(s)),

        // Placeholder — dead-code marker, emitted as opaque marker string
        CoreExpr::Placeholder => Ok("_".to_string()),

        // Rest parameter reference
        CoreExpr::Rest(name) => match name {
            Some(n) => Ok(format!("...{}", n)),
            None => Ok("...".to_string()),
        },

        // Variable references — apply substitution or rename
        CoreExpr::Var { name, .. } => {
            if param_scope.contains(name.as_str()) {
                // Bound by an enclosing param — apply rename if needed, otherwise leave
                Ok(rename_map
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.clone()))
            } else if let Some(scn) = substitutions.get(name) {
                // Captured from user env — inline the SCN
                Ok(scn.clone())
            } else {
                // Stdlib reference or ambient binding — leave as-is
                Ok(rename_map
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| name.clone()))
            }
        }

        // Annotated Var (Var { annotation: Some(_) }) is handled by the Var arm above.

        // Sequential: emit each expression in order, progressively injecting dict-level
        // bindings into scope so that later expressions can reference names introduced by
        // earlier dict expressions without treating them as free variables that need
        // substitution. Mirrors the resolver's sequential scope injection (resolve.rs:118-134).
        CoreExpr::Sequential(exprs) => {
            let mut seq_scope = param_scope.clone();
            let mut parts = Vec::with_capacity(exprs.len());
            for (i, e) in exprs.iter().enumerate() {
                parts.push(core_expr_to_tinct(
                    &e.node,
                    &seq_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?);
                // After all but the last expression, inject static dict keys into scope
                // so subsequent expressions do not substitute these names.
                if i + 1 < exprs.len() {
                    if let CoreExpr::Dict(entries) = &e.node {
                        for entry in entries {
                            if let Some(key) = &entry.node.key {
                                if core_expr_is_static_key(&key.node) {
                                    if let CoreExpr::Str(name) = &key.node {
                                        seq_scope.insert(name.clone());
                                    // Annotated Var (Var { annotation: Some(_) }) is a static key.
                                    } else if let CoreExpr::Var {
                                        name,
                                        annotation: Some(_),
                                        ..
                                    } = &key.node
                                    {
                                        seq_scope.insert(name.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(parts.join("\n"))
        }

        // Dict literal: `[k: v  ...]`
        // Keys are serialized in the parent scope (key isolation invariant).
        // Values are serialized with the letrec dict scope extended by all static key
        // names, mirroring eval_dict_core / the resolver (resolve.rs:83-97, eval_dict.rs:66-68).
        // This prevents dict-local variable references from being treated as free variables
        // and incorrectly replaced by substitution.
        CoreExpr::Dict(entries) => {
            // Collect static key names first to build the letrec dict scope.
            let mut dict_scope = param_scope.clone();
            for entry in entries {
                if let Some(key) = &entry.node.key {
                    if core_expr_is_static_key(&key.node) {
                        if let CoreExpr::Str(name) = &key.node {
                            dict_scope.insert(name.clone());
                        // Annotated Var (Var { annotation: Some(_) }) is a static key.
                        } else if let CoreExpr::Var {
                            name,
                            annotation: Some(_),
                            ..
                        } = &key.node
                        {
                            dict_scope.insert(name.clone());
                        }
                    }
                }
            }
            let mut out = String::from("[");
            for (i, entry) in entries.iter().enumerate() {
                if i > 0 {
                    out.push_str("  ");
                }
                if let Some(key) = &entry.node.key {
                    // Keys use parent scope (key isolation invariant).
                    let key_str =
                        core_expr_to_tinct(&key.node, param_scope, substitutions, rename_map, ctx)?;
                    out.push_str(&key_str);
                    out.push(':');
                    out.push(' ');
                }
                // Values use the letrec dict scope.
                let val_str = core_expr_to_tinct(
                    &entry.node.value.node,
                    &dict_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?;
                out.push_str(&val_str);
            }
            out.push(']');
            Ok(out)
        }

        // Call: `[func arg1 arg2 named_key: val]`
        CoreExpr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            let func_str =
                core_expr_to_tinct(&func.node, param_scope, substitutions, rename_map, ctx)?;
            let mut parts = vec![func_str];
            for arg in args {
                parts.push(core_expr_to_tinct(
                    &arg.node,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?);
            }
            for named_arg in named_args {
                let val_str = core_expr_to_tinct(
                    &named_arg.node.value.node,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?;
                parts.push(format!("{}: {}", named_arg.node.name, val_str));
            }
            Ok(format!("[{}]", parts.join(" ")))
        }

        // Nested Fn: `[fn [let params] body]`
        // Extend param scope, check for capture conflicts, recurse.
        CoreExpr::Fn { params, body, .. } => {
            let mut inner_scope = param_scope.clone();
            for p in params {
                inner_scope.insert(p.node.name.clone());
            }

            // Check capture avoidance for inner params against the outer substitutions.
            // Uses structural identifier extraction (not substring containment) to avoid
            // false positives from identifier substrings.
            let sub_text: String = substitutions
                .values()
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            let sub_idents = extract_identifiers(&sub_text);
            let mut inner_rename = rename_map.clone();
            for p in params {
                if sub_idents.contains(p.node.name.as_str())
                    && !inner_rename.contains_key(&p.node.name)
                {
                    let fresh = crate::builtins_meta::gensym_fresh('𝒻', &p.node.name);
                    inner_rename.insert(p.node.name.clone(), fresh);
                }
            }

            let body_str =
                core_expr_to_tinct(&body.node, &inner_scope, substitutions, &inner_rename, ctx)?;
            let params_str: Vec<String> = params
                .iter()
                .map(|p| {
                    let name = inner_rename
                        .get(&p.node.name)
                        .cloned()
                        .unwrap_or_else(|| p.node.name.clone());
                    if p.node.variadic {
                        format!("...{}", name)
                    } else {
                        name
                    }
                })
                .collect();
            Ok(format!("[fn [let {}] {}]", params_str.join(" "), body_str))
        }

        // TypeAssert: pass through to the inner expression — the type annotation is
        // not emitted in SCN (the consumer re-checks types in their own context).
        CoreExpr::TypeAssert { expr, .. } => {
            core_expr_to_tinct(&expr.node, param_scope, substitutions, rename_map, ctx)
        }

        // Match: `[match scrutinee [pattern: body] ...]`
        CoreExpr::Match { scrutinee, arms } => {
            let scrutinee_str =
                core_expr_to_tinct(&scrutinee.node, param_scope, substitutions, rename_map, ctx)?;
            let mut arm_parts = Vec::with_capacity(arms.len());
            for arm in arms {
                let pattern_str = format!("{}", arm.pattern.expr);
                let body_str = core_expr_to_tinct(
                    &arm.body.node,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?;

                if let Some(guard) = &arm.guard {
                    let guard_str = core_expr_to_tinct(
                        &guard.node,
                        param_scope,
                        substitutions,
                        rename_map,
                        ctx,
                    )?;
                    arm_parts.push(format!(
                        "[{}: [if {} {} []]]",
                        pattern_str, guard_str, body_str
                    ));
                } else {
                    arm_parts.push(format!("[{}: {}]", pattern_str, body_str));
                }
            }
            Ok(format!(
                "[match {}  {}]",
                scrutinee_str,
                arm_parts.join("  ")
            ))
        }

        // Quote: emit as-is — we do not substitute inside quotes.
        // Unquote sub-expressions ARE emitted with substitution applied.
        CoreExpr::Quote(inner) => {
            let inner_str = core_expr_to_tinct_in_quote(
                &inner.node,
                1,
                param_scope,
                substitutions,
                rename_map,
                ctx,
            )?;
            Ok(format!("(quote {})", inner_str))
        }

        CoreExpr::Unquote(inner) => {
            let inner_str =
                core_expr_to_tinct(&inner.node, param_scope, substitutions, rename_map, ctx)?;
            Ok(format!("(unquote {})", inner_str))
        }

        CoreExpr::UnquoteSplice(inner) => {
            let inner_str =
                core_expr_to_tinct(&inner.node, param_scope, substitutions, rename_map, ctx)?;
            Ok(format!("(unquote-splice {})", inner_str))
        }

        CoreExpr::PatternDecl { bindings } => {
            let mut parts = Vec::with_capacity(bindings.len());
            for b in bindings {
                parts.push(core_expr_to_tinct(
                    &b.node,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?);
            }
            Ok(format!("[pattern {}]", parts.join(" ")))
        }

        CoreExpr::LetDecl { bindings } => {
            let mut parts = Vec::with_capacity(bindings.len());
            for b in bindings {
                parts.push(core_expr_to_tinct(
                    &b.node,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?);
            }
            Ok(format!("[let {}]", parts.join(" ")))
        }

        CoreExpr::CaseArm {
            let_bindings,
            pattern,
            body,
        } => {
            // Extract binding names from let_bindings to build the arm scope.
            let mut arm_scope = param_scope.clone();
            if let CoreExpr::LetDecl { bindings } = &let_bindings.node {
                for binding in bindings {
                    if let CoreExpr::Str(name) = &binding.node {
                        arm_scope.insert(name.clone());
                    // Annotated Var (Var { annotation: Some(_) }) is also handled by Var arm.
                    } else if let CoreExpr::Var { name, .. } = &binding.node {
                        arm_scope.insert(name.clone());
                    }
                }
            }

            let lb_str = core_expr_to_tinct(
                &let_bindings.node,
                param_scope,
                substitutions,
                rename_map,
                ctx,
            )?;
            let pattern_str =
                core_expr_to_tinct(&pattern.node, param_scope, substitutions, rename_map, ctx)?;
            let body_str =
                core_expr_to_tinct(&body.node, &arm_scope, substitutions, rename_map, ctx)?;
            Ok(format!("[case {} {} {}]", lb_str, pattern_str, body_str))
        }

        // Variant: `TypeName.CtorName` or `[TypeName.CtorName payload]`
        CoreExpr::Variant { tag, payload } => {
            if let Some(inner) = payload {
                let inner_str =
                    core_expr_to_tinct(&inner.node, param_scope, substitutions, rename_map, ctx)?;
                Ok(format!("[{} {}]", tag, inner_str))
            } else {
                Ok(tag.clone())
            }
        }

        CoreExpr::UnitVariant { tycon, ctor } => {
            if tycon.is_empty() {
                Ok(ctor.clone())
            } else {
                Ok(format!("{}.{}", tycon, ctor))
            }
        }
    }
}

/// Serialize a `CoreExpr` inside a `Quote` context, tracking depth.
/// At depth > 0, variables are NOT substituted (they are opaque AST data).
/// Unquote/UnquoteSplice at depth 1 return to normal evaluation context.
fn core_expr_to_tinct_in_quote(
    expr: &CoreExpr,
    depth: usize,
    param_scope: &HashSet<String>,
    substitutions: &HashMap<String, String>,
    rename_map: &HashMap<String, String>,
    ctx: &Arc<EvalContext>,
) -> Result<String, String> {
    match expr {
        CoreExpr::Quote(inner) => {
            let s = core_expr_to_tinct_in_quote(
                &inner.node,
                depth + 1,
                param_scope,
                substitutions,
                rename_map,
                ctx,
            )?;
            Ok(format!("(quote {})", s))
        }
        CoreExpr::Unquote(inner) => {
            if depth == 1 {
                let s =
                    core_expr_to_tinct(&inner.node, param_scope, substitutions, rename_map, ctx)?;
                Ok(format!("(unquote {})", s))
            } else {
                let s = core_expr_to_tinct_in_quote(
                    &inner.node,
                    depth - 1,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?;
                Ok(format!("(unquote {})", s))
            }
        }
        CoreExpr::UnquoteSplice(inner) => {
            if depth == 1 {
                let s =
                    core_expr_to_tinct(&inner.node, param_scope, substitutions, rename_map, ctx)?;
                Ok(format!("(unquote-splice {})", s))
            } else {
                let s = core_expr_to_tinct_in_quote(
                    &inner.node,
                    depth - 1,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?;
                Ok(format!("(unquote-splice {})", s))
            }
        }
        // Inside quotes: emit as-is, recurse to find nested unquotes
        other => core_expr_to_tinct_raw(other, depth, param_scope, substitutions, rename_map, ctx),
    }
}

/// Emit a `CoreExpr` inside a `Quote` without applying substitution, but recursing
/// to find nested `Unquote`/`UnquoteSplice` sub-expressions.
fn core_expr_to_tinct_raw(
    expr: &CoreExpr,
    depth: usize,
    param_scope: &HashSet<String>,
    substitutions: &HashMap<String, String>,
    rename_map: &HashMap<String, String>,
    ctx: &Arc<EvalContext>,
) -> Result<String, String> {
    match expr {
        CoreExpr::Int(n) => Ok(fmt_int(*n)),
        CoreExpr::U64(n) => Ok(format!("{n}u")),
        CoreExpr::Float(f) => fmt_float(*f),
        CoreExpr::Str(s) => Ok(fmt_string(s)),
        CoreExpr::Placeholder => Ok("_".to_string()),
        CoreExpr::Rest(name) => match name {
            Some(n) => Ok(format!("...{}", n)),
            None => Ok("...".to_string()),
        },
        // Inside quotes: variables are opaque AST data — emit their names as-is.
        // Annotated Var (Var { annotation: Some(_) }) also uses the name field.
        CoreExpr::Var { name, .. } => Ok(name.clone()),

        CoreExpr::Sequential(exprs) => {
            let mut parts = Vec::new();
            for e in exprs {
                parts.push(core_expr_to_tinct_in_quote(
                    &e.node,
                    depth,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?);
            }
            Ok(parts.join("\n"))
        }
        CoreExpr::Dict(entries) => {
            let mut out = String::from("[");
            for (i, entry) in entries.iter().enumerate() {
                if i > 0 {
                    out.push_str("  ");
                }
                if let Some(key) = &entry.node.key {
                    let ks = core_expr_to_tinct_in_quote(
                        &key.node,
                        depth,
                        param_scope,
                        substitutions,
                        rename_map,
                        ctx,
                    )?;
                    out.push_str(&ks);
                    out.push(':');
                    out.push(' ');
                }
                let vs = core_expr_to_tinct_in_quote(
                    &entry.node.value.node,
                    depth,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?;
                out.push_str(&vs);
            }
            out.push(']');
            Ok(out)
        }
        CoreExpr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            let fs = core_expr_to_tinct_in_quote(
                &func.node,
                depth,
                param_scope,
                substitutions,
                rename_map,
                ctx,
            )?;
            let mut parts = vec![fs];
            for arg in args {
                parts.push(core_expr_to_tinct_in_quote(
                    &arg.node,
                    depth,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?);
            }
            for named_arg in named_args {
                let vs = core_expr_to_tinct_in_quote(
                    &named_arg.node.value.node,
                    depth,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?;
                parts.push(format!("{}: {}", named_arg.node.name, vs));
            }
            Ok(format!("[{}]", parts.join(" ")))
        }
        CoreExpr::Fn { params, body, .. } => {
            let bs = core_expr_to_tinct_in_quote(
                &body.node,
                depth,
                param_scope,
                substitutions,
                rename_map,
                ctx,
            )?;
            let ps: Vec<String> = params.iter().map(|p| p.node.name.clone()).collect();
            Ok(format!("[fn [let {}] {}]", ps.join(" "), bs))
        }
        CoreExpr::TypeAssert { expr, .. } => core_expr_to_tinct_in_quote(
            &expr.node,
            depth,
            param_scope,
            substitutions,
            rename_map,
            ctx,
        ),
        CoreExpr::Match { scrutinee, arms } => {
            let ss = core_expr_to_tinct_in_quote(
                &scrutinee.node,
                depth,
                param_scope,
                substitutions,
                rename_map,
                ctx,
            )?;
            let mut arm_parts = Vec::new();
            for arm in arms {
                let ps = format!("{}", arm.pattern.expr);
                let bs = core_expr_to_tinct_in_quote(
                    &arm.body.node,
                    depth,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?;
                arm_parts.push(format!("[{}: {}]", ps, bs));
            }
            Ok(format!("[match {}  {}]", ss, arm_parts.join("  ")))
        }
        CoreExpr::Quote(inner) => {
            let s = core_expr_to_tinct_in_quote(
                &inner.node,
                depth + 1,
                param_scope,
                substitutions,
                rename_map,
                ctx,
            )?;
            Ok(format!("(quote {})", s))
        }
        // These arms are unreachable: `core_expr_to_tinct_in_quote` matches
        // Unquote/UnquoteSplice explicitly before delegating to `core_expr_to_tinct_raw`
        // via the `other =>` catch-all. They are kept for exhaustiveness only.
        CoreExpr::Unquote(inner) => {
            let s = core_expr_to_tinct_in_quote(
                &inner.node,
                depth - 1,
                param_scope,
                substitutions,
                rename_map,
                ctx,
            )?;
            Ok(format!("(unquote {})", s))
        }
        CoreExpr::UnquoteSplice(inner) => {
            let s = core_expr_to_tinct_in_quote(
                &inner.node,
                depth - 1,
                param_scope,
                substitutions,
                rename_map,
                ctx,
            )?;
            Ok(format!("(unquote-splice {})", s))
        }
        CoreExpr::PatternDecl { bindings } => {
            let mut parts = Vec::new();
            for b in bindings {
                parts.push(core_expr_to_tinct_in_quote(
                    &b.node,
                    depth,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?);
            }
            Ok(format!("[pattern {}]", parts.join(" ")))
        }
        CoreExpr::LetDecl { bindings } => {
            let mut parts = Vec::new();
            for b in bindings {
                parts.push(core_expr_to_tinct_in_quote(
                    &b.node,
                    depth,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?);
            }
            Ok(format!("[let {}]", parts.join(" ")))
        }
        CoreExpr::CaseArm {
            let_bindings,
            pattern,
            body,
        } => {
            // Extract binding names from let_bindings to build the arm scope.
            let mut arm_scope = param_scope.clone();
            if let CoreExpr::LetDecl { bindings } = &let_bindings.node {
                for binding in bindings {
                    if let CoreExpr::Str(name) = &binding.node {
                        arm_scope.insert(name.clone());
                    // Annotated Var (Var { annotation: Some(_) }) is also handled by Var arm.
                    } else if let CoreExpr::Var { name, .. } = &binding.node {
                        arm_scope.insert(name.clone());
                    }
                }
            }

            let lb_s = core_expr_to_tinct_in_quote(
                &let_bindings.node,
                depth,
                param_scope,
                substitutions,
                rename_map,
                ctx,
            )?;
            let ps = core_expr_to_tinct_in_quote(
                &pattern.node,
                depth,
                param_scope,
                substitutions,
                rename_map,
                ctx,
            )?;
            let bs = core_expr_to_tinct_in_quote(
                &body.node,
                depth,
                &arm_scope,
                substitutions,
                rename_map,
                ctx,
            )?;
            Ok(format!("[case {} {} {}]", lb_s, ps, bs))
        }

        // Variant: tag is opaque AST data inside quotes — emit as-is.
        CoreExpr::Variant { tag, payload } => {
            if let Some(inner) = payload {
                let s = core_expr_to_tinct_in_quote(
                    &inner.node,
                    depth,
                    param_scope,
                    substitutions,
                    rename_map,
                    ctx,
                )?;
                Ok(format!("[{} {}]", tag, s))
            } else {
                Ok(tag.clone())
            }
        }

        CoreExpr::UnitVariant { tycon, ctor } => {
            if tycon.is_empty() {
                Ok(ctor.clone())
            } else {
                Ok(format!("{}.{}", tycon, ctor))
            }
        }
    }
}

// serialize_pattern deleted (T-1750) — patterns are now SurfaceNode, not Pattern enum.

// ────────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────────

/// Check if a string is a valid tinct bare identifier.
///
/// Valid identifiers match: [a-zA-Z_][a-zA-Z0-9_-?]*
/// Reserved words (`let`, `case`, `true`, `false`) are not valid bare identifiers.
fn is_valid_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }

    // Reserved words must be quoted
    if s == "let" || s == "case" || s == "true" || s == "false" {
        return false;
    }

    let mut chars = s.chars();
    let first = chars.next().unwrap();

    // First char must be ASCII letter or underscore
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }

    // Remaining chars must be ASCII alphanumeric, underscore, hyphen, or question mark
    for c in chars {
        if !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '?' {
            return false;
        }
    }

    true
}

// ────────────────────────────────────────────────────────────────────────────────
// Value::to_tinct implementation
// ────────────────────────────────────────────────────────────────────────────────

impl Value {
    /// Serialize a Value to its Self-Contained Normal Form (SCN) representation.
    ///
    /// Returns a tinct source string that, when parsed and evaluated in a stdlib environment,
    /// produces an equivalent value.
    ///
    /// `ctx` is `None` when the caller guarantees no Function values are present (e.g.,
    /// profiling path serializing scalar dicts). Function serialization requires `Some(ctx)`
    /// for stdlib membership checks.
    ///
    /// Returns an error for values with no tinct representation (capabilities, tasks, etc.).
    pub fn to_tinct(&self, ctx: Option<&Arc<EvalContext>>) -> Result<String, String> {
        match self {
            Value::Int(n) => Ok(fmt_int(*n)),
            Value::Float(f) => fmt_float(*f),
            Value::String { source, start, end } => Ok(fmt_string(&source[*start..*end])),
            Value::Dict(map) => fmt_dict(map, ctx),
            Value::Variant {
                tycon,
                ctor,
                payload,
            } => fmt_variant(&format!("{}.{}", tycon, ctor), payload.clone(), ctx),
            Value::Builtin(b) => Ok(b.name.to_string()),
            Value::Function {
                params,
                body,
                closure_env: _,
                annotation: _,
            } => match ctx {
                Some(ctx) => {
                    // Value::Function no longer stores Arc<RwLock<Env>> (removed in T-1558).
                    // Uses a stub Env for fmt_fn formatting; B-515 tracks proper FlatEnv wiring.
                    let stub_env = Env::new();
                    fmt_fn(params, body, &stub_env, ctx)
                }
                None => Err("Function serialization requires EvalContext".to_string()),
            },
            Value::Decimal(d) => Ok(crate::lexer::fmt_decimal(d)),
            Value::BigInt(n) => Ok(crate::lexer::fmt_bigint(n)),
            Value::Bytes { source, start, end } => {
                Ok(crate::lexer::fmt_bytes(&source[*start..*end]))
            }
            Value::Timestamp(nanos) => Ok(format!("[timestamp-nanos {}]", nanos)),
            Value::Duration(nanos) => Ok(format!("[duration-nanos {}]", nanos)),
            // Non-serializable values — no tinct representation exists
            Value::DirCap { .. } => {
                Err(format!("no tinct representation for {}", self.type_name()))
            }
            Value::NetCap(_) => Err(format!("no tinct representation for {}", self.type_name())),
            Value::ClockCap(_) => Err(format!("no tinct representation for {}", self.type_name())),
            Value::RevocableDirCap { .. } => {
                Err(format!("no tinct representation for {}", self.type_name()))
            }
            Value::File(_) => Err(format!("no tinct representation for {}", self.type_name())),
            Value::QuicSession { .. } => {
                Err(format!("no tinct representation for {}", self.type_name()))
            }
            Value::Http2Session { .. } => {
                Err(format!("no tinct representation for {}", self.type_name()))
            }
            Value::Http3Session { .. } => {
                Err(format!("no tinct representation for {}", self.type_name()))
            }
            Value::QuicDatagramHandle { .. } => {
                Err(format!("no tinct representation for {}", self.type_name()))
            }
            Value::Task(_) => Err(format!("no tinct representation for {}", self.type_name())),
            Value::Channel(_) => Err(format!("no tinct representation for {}", self.type_name())),
            Value::Context(_) => Err(format!("no tinct representation for {}", self.type_name())),
            Value::Builder(_) => Err(format!("no tinct representation for {}", self.type_name())),
            Value::Proxy { .. } => Err(format!("no tinct representation for {}", self.type_name())),
            Value::Timezone(_) => Err(format!("no tinct representation for {}", self.type_name())),
            Value::Program { .. } => {
                Err(format!("no tinct representation for {}", self.type_name()))
            }
            Value::Document(_) => Err(format!("no tinct representation for {}", self.type_name())),
            Value::Uri { .. } => Err(format!("no tinct representation for {}", self.type_name())),
            Value::ReactiveCell(_) => {
                Err(format!("no tinct representation for {}", self.type_name()))
            }
            Value::BroadcastChannel(_) | Value::OneshotSender(_) | Value::OneshotReceiver(_) => {
                Err(format!("no tinct representation for {}", self.type_name()))
            }
            Value::U64(n) => Ok(format!("{n}u")),
            // Annotated is transparent — delegate to inner value.
            Value::Annotated { inner, .. } => inner.to_tinct(ctx),
            Value::TypeContext(_) => {
                Err(format!("no tinct representation for {}", self.type_name()))
            }
            Value::Bool(b) => Ok(if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }),
            Value::Seq { .. } => Err(format!("no tinct representation for {}", self.type_name())),
            Value::Expression(_) => {
                Err(format!("no tinct representation for {}", self.type_name()))
            }
            Value::Arena { .. } => Err(format!("no tinct representation for {}", self.type_name())),
            Value::CoreDocument { .. } => {
                Err(format!("no tinct representation for {}", self.type_name()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_identifier() {
        // Valid identifiers
        assert!(is_valid_identifier("x"));
        assert!(is_valid_identifier("foo"));
        assert!(is_valid_identifier("_priv"));
        assert!(is_valid_identifier("foo-bar"));
        assert!(is_valid_identifier("empty?"));
        assert!(is_valid_identifier("x123"));

        // Invalid identifiers
        assert!(!is_valid_identifier(""));
        assert!(!is_valid_identifier("123"));
        assert!(!is_valid_identifier("-foo"));
        assert!(!is_valid_identifier("?foo"));
        assert!(!is_valid_identifier("foo.bar")); // dot not allowed
        assert!(!is_valid_identifier("foo bar")); // space not allowed

        // Reserved words
        assert!(!is_valid_identifier("let"));
        assert!(!is_valid_identifier("case"));
        assert!(!is_valid_identifier("true"));
        assert!(!is_valid_identifier("false"));
    }

    #[test]
    fn test_fmt_string() {
        assert_eq!(fmt_string("hello"), r#""hello""#);
        assert_eq!(fmt_string(""), r#""""#);
        assert_eq!(fmt_string("hello\nworld"), r#""hello\nworld""#);
        assert_eq!(fmt_string("tab\there"), r#""tab\there""#);
        assert_eq!(fmt_string(r#"quote"here"#), r#""quote\"here""#);
        assert_eq!(fmt_string(r"back\slash"), r#""back\\slash""#);
        assert_eq!(fmt_string("cr\rhere"), "\"cr\\rhere\"");
    }

    #[test]
    fn test_fmt_bytes() {
        use crate::lexer::fmt_bytes;
        assert_eq!(fmt_bytes(&[]), "[bytes-of []]");
        assert_eq!(fmt_bytes(&[42]), "[bytes-of [0: 42]]");
        assert_eq!(fmt_bytes(&[1, 2, 3]), "[bytes-of [0: 1  1: 2  2: 3]]");
    }

    #[test]
    fn test_extract_identifiers_basic() {
        let ids = extract_identifiers("x y z");
        assert!(ids.contains("x"));
        assert!(ids.contains("y"));
        assert!(ids.contains("z"));
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn test_extract_identifiers_no_substring_match() {
        // "x-coordinate" is a single identifier token (hyphen is not a delimiter),
        // so "x" should NOT appear in the extracted set.
        let ids = extract_identifiers("x-coordinate");
        assert!(ids.contains("x-coordinate"));
        assert!(!ids.contains("x"));
        assert_eq!(ids.len(), 1);
    }

    #[test]
    fn test_extract_identifiers_delimiters() {
        let ids = extract_identifiers("[+ x 1]");
        assert!(ids.contains("+"));
        assert!(ids.contains("x"));
        assert!(ids.contains("1"));
        assert_eq!(ids.len(), 3);
    }

    #[test]
    fn test_extract_identifiers_skips_string_literals() {
        // "x" inside a string literal should not be extracted.
        let ids = extract_identifiers(r#"[concat "x" y]"#);
        assert!(ids.contains("concat"));
        assert!(ids.contains("y"));
        assert!(!ids.contains("x")); // inside string literal
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn test_extract_identifiers_escaped_quotes_in_strings() {
        // String with escaped quote: "say \"hello\"" — the identifier after
        // the string should still be extracted.
        let ids = extract_identifiers(r#""say \"hello\"" z"#);
        assert!(ids.contains("z"));
        assert!(!ids.contains("say"));
        assert!(!ids.contains("hello"));
    }

    #[test]
    fn test_extract_identifiers_dot_separated() {
        // Dot is a delimiter, so "foo.bar" yields two identifiers.
        let ids = extract_identifiers("foo.bar");
        assert!(ids.contains("foo"));
        assert!(ids.contains("bar"));
        assert_eq!(ids.len(), 2);
    }

    #[test]
    fn test_extract_identifiers_empty() {
        let ids = extract_identifiers("");
        assert!(ids.is_empty());
    }

    #[test]
    fn test_extract_identifiers_colon_separated() {
        // "key: value" — colon is a delimiter
        let ids = extract_identifiers("key: value");
        assert!(ids.contains("key"));
        assert!(ids.contains("value"));
        assert_eq!(ids.len(), 2);
    }
}
