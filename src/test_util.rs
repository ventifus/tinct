//! Shared test helpers: `test_span()` and `sp()` for constructing test fixtures.

use crate::ast::{Span, Spanned};
use crate::rust_span;

pub fn test_span(start_line: u32, start_col: u32, end_line: u32, end_col: u32) -> Span {
    Span::new(start_line, start_col, end_line, end_col, rust_span!().file)
}

pub fn sp<T>(node: T) -> Spanned<T> {
    Spanned::new(node, test_span(1, 1, 1, 10))
}

/// Seed an `EvalContext` with caller-provided type display names.
///
/// Tests that assert on error messages containing type names (e.g., `"expected Int"`)
/// must seed their context with display names for the type IDs that appear in those
/// messages. Each test module defines only the names its tests assert on.
pub fn seed_test_type_context(
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
    names: &[(&str, u64)],
) {
    let name_to_id: std::collections::HashMap<String, u64> =
        names.iter().map(|&(n, id)| (n.to_string(), id)).collect();
    let display_names: std::collections::HashMap<u64, String> =
        names.iter().map(|&(n, id)| (id, n.to_string())).collect();
    ctx.init_type_context(crate::eval::TypeContextData {
        inference_env: std::sync::Arc::new(std::sync::RwLock::new(crate::env::Env::new())),
        tycon_env: std::collections::HashMap::new(),
        type_stage_scope: vec![],
        type_stage_fns: std::collections::HashMap::new(),
        type_stage_type_vars: std::collections::HashMap::new(),
        type_diagnostics: vec![],
        type_name_to_id: name_to_id,
        type_display_names: display_names,
    });
}

