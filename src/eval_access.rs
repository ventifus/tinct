//! Access expression evaluation: proxy handler invocation.
//!
//! This module contains `invoke_proxy_handler`, extracted
//! from `eval.rs` to keep that module focused on the core evaluation loop.

use std::sync::Arc;

use crate::ast::Span;
use crate::error::{EvalError, EvalResult};
use crate::eval::{materialize, EvalContext};
use crate::eval_call::{invoke_function, CallContext};
use crate::value::{Thunk, Value};

/// Invoke a proxy handler with a key value, returning the result thunk.
pub(crate) async fn invoke_proxy_handler(
    handler: &Arc<Thunk>,
    key_val: Value,
    caller_env_id: u32,
    ctx: &Arc<EvalContext>,
    access_span: &Span,
) -> EvalResult<Arc<Thunk>> {
    // Performance: handler thunk is memoized by Launchbury sharing, but each
    // access clones the materialized Value. Consider eager materialization in
    // builtin_proxy for hot proxy access.
    let handler_val = materialize(handler, Some(access_span), ctx).await?;
    let key_arg_id = Arc::new(Thunk::value(key_val, access_span.clone()));
    match handler_val {
        Value::Function {
            params,
            body,
            closure_env,
            ..
        } => {
            invoke_function(&CallContext {
                params: &params,
                body: &body,
                closure_env,
                positional: &[key_arg_id],
                named: None,
                call_span: access_span
                    .clone()
                    .with_name(Arc::from("proxy field access")),
                ctx,
            })
            .await
        }
        Value::Builtin(def) => Ok(Arc::new(Thunk::builtin_call(
            def,
            vec![key_arg_id],
            None,
            access_span
                .clone()
                .with_name(Arc::from("proxy field access")),
            caller_env_id,
            Arc::clone(ctx),
        ))),
        _ => Err(EvalError::type_mismatch(
            "Function or Builtin",
            handler_val.type_name(),
            access_span.clone(),
        )
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use indexmap::IndexMap;

    use crate::ast::{SurfaceDocument, SurfaceItem, SurfaceProgram};
    use crate::error::{EvalError, EvalResult};
    use crate::eval::EvalContext;
    use crate::value::{Thunk, Value};

    /// Build a fresh EvalContext for tests.
    ///
    /// `EvalContext::new` pre-populates the FlatEnv root scope with Value::Builtin thunks,
    /// providing a consistent name→slot mapping for tests that exercise builtin-dict-get
    /// (dot-access) and other builtins.
    fn core_env_and_ctx() -> Arc<EvalContext> {
        EvalContext::new()
    }

    /// Parse and evaluate a surface expression with the core env seeded into the
    /// resolver so builtin names resolve correctly.
    async fn eval_str(src: &str, ctx: &Arc<EvalContext>) -> EvalResult<Arc<Thunk>> {
        use crate::ast::Spanned;
        use crate::resolve::resolve_surface_program;
        let node = crate::parser::parse_surface_expression(src).map_err(|e| {
            Box::new(EvalError::internal(
                format!("parse_surface_expression({src:?}) failed: {e:?}"),
                crate::rust_span!(),
            ))
        })?;
        let span = node.span.clone();
        let doc = SurfaceDocument {
            header: IndexMap::new(),
            items: vec![SurfaceItem::Expr(Arc::clone(&node))],
        };
        let program = crate::desugar::desugar_program_full(&SurfaceProgram {
            documents: vec![Spanned::new(Arc::new(doc), span)],
        });
        // Seed resolver from the full root_group so all builtin slots match the runtime.
        let root_frame = ctx.root_group_resolver_map();
        let (_table, _frames) = resolve_surface_program(&program, &[root_frame]);
        crate::eval_surface_file(&program, ctx).await
    }

    async fn materialize(
        thunk: &Arc<Thunk>,
        ctx: &Arc<EvalContext>,
    ) -> crate::error::EvalResult<Value> {
        crate::eval::materialize(thunk, None, ctx).await
    }

    /// Dot access on an existing key returns the field's value.
    ///
    /// `[d: [a: 1]  result: d.a]` — `result` accesses key `a` from sibling `d`
    /// via letrec scope. After evaluation, `result` must be Int(1).
    ///
    /// This exercises the `builtin-dict-get` path which is dispatched for all dot-access
    /// expressions in the evaluator.
    #[tokio::test]
    async fn test_dot_access_existing_key() -> EvalResult<()> {
        let ctx = core_env_and_ctx();
        let thunk = eval_str("[d: [a: 1]  result: d.a]", &ctx).await?;
        let val = materialize(&thunk, &ctx).await?;

        let Value::Dict(map) = val else {
            return Err(Box::new(EvalError::internal(
                "expected Dict".to_string(),
                crate::rust_span!(),
            )));
        };
        let result_thunk = map
            .get(&crate::value::HashableValue::Str("result".into()))
            .cloned()
            .expect("key 'result' must exist");
        let result_val = crate::eval::materialize(&result_thunk, None, &ctx).await?;
        assert_eq!(
            result_val,
            Value::Int(1),
            "d.a must return Int(1) when d = {{a: 1}}"
        );
        Ok(())
    }

    /// Dot access on a missing key produces an error when the access is forced.
    ///
    /// `[d: [a: 1]  result: d.b]` — key "b" does not exist in `d`. Forcing `result`
    /// must produce an error, not a silent absent/empty value.
    #[tokio::test]
    async fn test_dot_access_missing_key() -> EvalResult<()> {
        let ctx = core_env_and_ctx();
        let thunk = eval_str("[d: [a: 1]  result: d.b]", &ctx).await?;

        // The outer dict materializes fine; forcing `result` triggers the access error.
        let outer_val = materialize(&thunk, &ctx).await?;
        let Value::Dict(map) = outer_val else {
            return Err(Box::new(EvalError::internal(
                "expected Dict".to_string(),
                crate::rust_span!(),
            )));
        };
        let result_thunk = map
            .get(&crate::value::HashableValue::Str("result".into()))
            .cloned()
            .expect("key 'result' must exist in outer dict");
        let result = crate::eval::materialize(&result_thunk, None, &ctx).await;

        assert!(
            result.is_err(),
            "dot access d.b when d has no key 'b' must produce an error"
        );
        let err = result.unwrap_err();
        let msg = err.kind.to_string();
        // Error must reference the missing key or say "not found" / "missing".
        assert!(
            msg.contains('b') || msg.contains("not found") || msg.contains("missing"),
            "error must mention the missing key 'b' or 'not found': {msg}"
        );
        Ok(())
    }

    /// Integer-key dot access: `[d: ["x"]  result: d.0]` → String("x").
    ///
    /// Auto-indexed list `["x"]` creates key 0 → "x". Dot access `.0` desugars to
    /// `builtin-dict-get` with an integer key. The result must be String("x").
    #[tokio::test]
    async fn test_bracket_access() -> EvalResult<()> {
        let ctx = core_env_and_ctx();
        let thunk = eval_str("[d: [\"x\"]  result: d.0]", &ctx).await?;
        let outer_val = materialize(&thunk, &ctx).await?;

        let Value::Dict(map) = outer_val else {
            return Err(Box::new(EvalError::internal(
                "expected Dict".to_string(),
                crate::rust_span!(),
            )));
        };
        let result_thunk = map
            .get(&crate::value::HashableValue::Str("result".into()))
            .cloned()
            .expect("key 'result' must exist");
        let result_val = crate::eval::materialize(&result_thunk, None, &ctx).await?;
        assert_eq!(
            result_val,
            crate::value::string_val("x"),
            "d.0 on [\"x\"] must return String(\"x\")"
        );
        Ok(())
    }
}
