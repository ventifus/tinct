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
    let key_arg = Arc::new(Thunk::value(key_val, access_span.clone()));
    let key_arg_id = ctx.alloc_thunk(0, key_arg);
    match handler_val {
        Value::Function {
            params,
            body,
            closure_env_id,
            ..
        } => {
            invoke_function(&CallContext {
                params: &params,
                body: &body,
                closure_env_id,
                positional: &[key_arg_id],
                named: None,
                default_env_id: closure_env_id,
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
    use std::sync::{Arc, RwLock};

    use indexmap::IndexMap;

    use crate::ast::{SurfaceDocument, SurfaceItem, SurfaceProgram};
    use crate::error::EvalResult;
    use crate::eval::EvalContext;
    use crate::value::{Thunk, Value};

    /// Build a core env + fresh EvalContext.
    ///
    /// `build_core_env()` returns `Arc<RwLock<Env>>` with builtin names registered
    /// for the resolver. `EvalContext::new` pre-populates the FlatEnv root scope with
    /// the matching Value::Builtin thunks. Together they provide a consistent name→slot
    /// mapping for tests that exercise field-get / slot-get builtins.
    fn core_env_and_ctx() -> (Arc<RwLock<crate::env::Env>>, Arc<EvalContext>) {
        let env = crate::builtins::build_core_env(); // Arc<RwLock<Env>>
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        let ctx = EvalContext::new(base_dir, false);
        (env, ctx)
    }

    /// Parse and evaluate a surface expression with the core env seeded into the
    /// resolver so builtin names ($field-get, $slot-get, etc.) resolve correctly.
    async fn eval_str(
        src: &str,
        env: Arc<RwLock<crate::env::Env>>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        use crate::ast::Spanned;
        use crate::desugar::desugar_surface_program;
        use crate::resolve::resolve_surface_program;
        let node = crate::parser::parse_surface_expression(src)
            .unwrap_or_else(|e| panic!("parse_surface_expression({src:?}) failed: {e:?}"));
        let span = node.span.clone();
        let doc = SurfaceDocument {
            header: IndexMap::new(),
            items: vec![SurfaceItem::Expr(Arc::clone(&node))],
        };
        let program = SurfaceProgram {
            documents: vec![Spanned::new(Arc::new(doc), span)],
        };
        let mut program = program;
        desugar_surface_program(&mut program);
        // Seed the resolver from the FlatEnv root scope so $field-get and $slot-get
        // are available for dot-access desugaring (installed by build_core_env).
        let root_frame: IndexMap<String, u32> = crate::builtins_core::core_builtins()
            .iter()
            .enumerate()
            .map(|(i, def)| (def.name.to_string(), i as u32))
            .collect();
        let _ = env; // env is legacy; real bindings live in FlatEnv
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
    /// This exercises the field-get builtin which is dispatched for all dot-access
    /// expressions in the evaluator.
    #[tokio::test]
    async fn test_dot_access_existing_key() {
        let (env, ctx) = core_env_and_ctx();
        let thunk = eval_str("[d: [a: 1]  result: d.a]", env, &ctx)
            .await
            .unwrap();
        let val = materialize(&thunk, &ctx).await.unwrap();

        let Value::Dict(map) = val else {
            panic!("expected Dict, got: {val:?}");
        };
        let result_id = *map
            .get(&crate::value::HashableValue::Str("result".into()))
            .expect("key 'result' must exist");
        let result_val = crate::eval::materialize(&ctx.get_thunk(result_id), None, &ctx)
            .await
            .unwrap();
        assert_eq!(
            result_val,
            Value::Int(1),
            "d.a must return Int(1) when d = {{a: 1}}"
        );
    }

    /// Dot access on a missing key produces an error when the access is forced.
    ///
    /// `[d: [a: 1]  result: d.b]` — key "b" does not exist in `d`. Forcing `result`
    /// must produce an error, not a silent absent/empty value.
    #[tokio::test]
    async fn test_dot_access_missing_key() {
        let (env, ctx) = core_env_and_ctx();
        let thunk = eval_str("[d: [a: 1]  result: d.b]", env, &ctx)
            .await
            .unwrap();

        // The outer dict materializes fine; forcing `result` triggers the access error.
        let outer_val = materialize(&thunk, &ctx).await.unwrap();
        let Value::Dict(map) = outer_val else {
            panic!("expected Dict, got: {outer_val:?}");
        };
        let result_id = *map
            .get(&crate::value::HashableValue::Str("result".into()))
            .expect("key 'result' must exist in outer dict");
        let result = crate::eval::materialize(&ctx.get_thunk(result_id), None, &ctx).await;

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
    }

    /// Integer-key dot access: `[d: ["x"]  result: d.0]` → String("x").
    ///
    /// Auto-indexed list `["x"]` creates key 0 → "x". Dot access `.0` uses the
    /// integer-key (slot-get) path in the evaluator. The result must be String("x").
    #[tokio::test]
    async fn test_bracket_access() {
        let (env, ctx) = core_env_and_ctx();
        let thunk = eval_str("[d: [\"x\"]  result: d.0]", env, &ctx)
            .await
            .unwrap();
        let outer_val = materialize(&thunk, &ctx).await.unwrap();

        let Value::Dict(map) = outer_val else {
            panic!("expected Dict, got: {outer_val:?}");
        };
        let result_id = *map
            .get(&crate::value::HashableValue::Str("result".into()))
            .expect("key 'result' must exist");
        let result_val = crate::eval::materialize(&ctx.get_thunk(result_id), None, &ctx)
            .await
            .unwrap();
        assert_eq!(
            result_val,
            crate::value::string_val("x"),
            "d.0 on [\"x\"] must return String(\"x\")"
        );
    }
}
