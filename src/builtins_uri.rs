//! URI parsing builtins: uri, url, urn
//!
//! Implements RFC 3986 (URI) and RFC 8141 (URN) parsing. Uses the `url` crate for
//! hierarchical URL parsing, with manual fallback for non-hierarchical URIs.

use std::rc::Rc;

use indexmap::IndexMap;

use crate::builtins::{expect_one_arg, ok_val};
use crate::error::{EvalError, EvalResult};
use crate::value::{string_val, BuiltinArgs, Key, Thunk, Value};

/// Parse any URI string → Uri dict
///
/// Returns a Dict with: scheme, username, password, host, port, path, query, fragment.
/// host/port are null for non-hierarchical URIs (mailto:, tel:, urn:, news:).
/// username/password extracted by splitting userinfo on ":" (RFC 3986 convention).
pub(crate) fn builtin_uri(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    let val = expect_one_arg("uri", args, named, &ctx, depth, call_span)?;
    let s = match val {
        Value::String {
            ref source,
            start,
            end,
        } => &source[start..end],
        _ => {
            return Err(EvalError::type_mismatch("String", val.type_name(), call_span).into());
        }
    };

    // Try parsing with url::Url first (handles hierarchical URIs)
    if let Ok(parsed) = url::Url::parse(s) {
        let mut dict = IndexMap::new();

        // scheme (lowercase)
        dict.insert(
            Key::String("scheme".to_string()),
            ctx.alloc_thunk(ok_val(string_val(parsed.scheme()), call_span)?),
        );

        // username (split from userinfo)
        let username = if parsed.username().is_empty() {
            Value::Dict(IndexMap::new())
        } else {
            string_val(parsed.username())
        };
        dict.insert(
            Key::String("username".to_string()),
            ctx.alloc_thunk(ok_val(username, call_span)?),
        );

        // password (split from userinfo)
        let password = match parsed.password() {
            Some(pw) => string_val(pw),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            Key::String("password".to_string()),
            ctx.alloc_thunk(ok_val(password, call_span)?),
        );

        // host (null for non-hierarchical; strip IPv6 brackets)
        let host = match parsed.host_str() {
            Some(h) => string_val(h),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            Key::String("host".to_string()),
            ctx.alloc_thunk(ok_val(host, call_span)?),
        );

        // port (null if not specified)
        let port = match parsed.port() {
            Some(p) => Value::Int(i64::from(p)),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            Key::String("port".to_string()),
            ctx.alloc_thunk(ok_val(port, call_span)?),
        );

        // path (always present per RFC 3986)
        dict.insert(
            Key::String("path".to_string()),
            ctx.alloc_thunk(ok_val(string_val(parsed.path()), call_span)?),
        );

        // query (null if absent)
        let query = match parsed.query() {
            Some(q) => string_val(q),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            Key::String("query".to_string()),
            ctx.alloc_thunk(ok_val(query, call_span)?),
        );

        // fragment (null if absent)
        let fragment = match parsed.fragment() {
            Some(f) => string_val(f),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            Key::String("fragment".to_string()),
            ctx.alloc_thunk(ok_val(fragment, call_span)?),
        );

        return ok_val(Value::Dict(dict), call_span);
    }

    // Fallback: manual parsing for non-hierarchical URIs (mailto:, tel:, urn:, news:)
    // These don't have authority (host/port), so url::Url rejects them.
    let (scheme, rest) = match s.split_once(':') {
        Some((scheme, rest)) => (scheme, rest),
        None => {
            return Err(
                EvalError::uri_parse_error(format!("missing scheme: {}", s), call_span).into(),
            );
        }
    };

    let mut dict = IndexMap::new();

    dict.insert(
        Key::String("scheme".to_string()),
        ctx.alloc_thunk(ok_val(string_val(&scheme.to_lowercase()), call_span)?),
    );

    // Non-hierarchical URIs: all null for userinfo/host/port
    for key in ["username", "password", "host", "port"] {
        dict.insert(
            Key::String(key.to_string()),
            ctx.alloc_thunk(ok_val(Value::Dict(IndexMap::new()), call_span)?),
        );
    }

    // path is the remaining part after scheme:
    // For mailto:user@example.com, path is "user@example.com"
    // For urn:isbn:123, path is "isbn:123"
    dict.insert(
        Key::String("path".to_string()),
        ctx.alloc_thunk(ok_val(string_val(rest), call_span)?),
    );

    // query and fragment: null (non-hierarchical URIs typically don't have these)
    for key in ["query", "fragment"] {
        dict.insert(
            Key::String(key.to_string()),
            ctx.alloc_thunk(ok_val(Value::Dict(IndexMap::new()), call_span)?),
        );
    }

    ok_val(Value::Dict(dict), call_span)
}

/// Parse hierarchical URL → Url dict
///
/// Errors if no authority (no host). Port defaults to scheme default if not specified.
pub(crate) fn builtin_url(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    let val = expect_one_arg("url", args, named, &ctx, depth, call_span)?;
    let s = match val {
        Value::String {
            ref source,
            start,
            end,
        } => &source[start..end],
        _ => {
            return Err(EvalError::type_mismatch("String", val.type_name(), call_span).into());
        }
    };

    let parsed = url::Url::parse(s)
        .map_err(|e| EvalError::uri_parse_error(format!("invalid URL: {}", e), call_span))?;

    // Reject non-hierarchical URIs (no authority)
    if parsed.host_str().is_none() {
        return Err(EvalError::uri_parse_error(
            format!("URL requires host (got non-hierarchical URI): {}", s),
            call_span,
        )
        .into());
    }

    let mut dict = IndexMap::new();

    // scheme (lowercase)
    dict.insert(
        Key::String("scheme".to_string()),
        ctx.alloc_thunk(ok_val(string_val(parsed.scheme()), call_span)?),
    );

    // username (split from userinfo)
    let username = if parsed.username().is_empty() {
        Value::Dict(IndexMap::new())
    } else {
        string_val(parsed.username())
    };
    dict.insert(
        Key::String("username".to_string()),
        ctx.alloc_thunk(ok_val(username, call_span)?),
    );

    // password (split from userinfo)
    let password = match parsed.password() {
        Some(pw) => string_val(pw),
        None => Value::Dict(IndexMap::new()),
    };
    dict.insert(
        Key::String("password".to_string()),
        ctx.alloc_thunk(ok_val(password, call_span)?),
    );

    // host (always present for URLs; unwrap is safe)
    dict.insert(
        Key::String("host".to_string()),
        ctx.alloc_thunk(ok_val(string_val(parsed.host_str().unwrap()), call_span)?),
    );

    // port (default to scheme default if not specified)
    let port = parsed.port_or_known_default().unwrap_or_else(|| {
        // Fallback for unknown schemes: return port 0 as sentinel
        // (url::Url::port_or_known_default returns None for unknown schemes)
        0
    });
    dict.insert(
        Key::String("port".to_string()),
        ctx.alloc_thunk(ok_val(Value::Int(i64::from(port)), call_span)?),
    );

    // path (always present per RFC 3986; default to "/" if empty)
    let path = if parsed.path().is_empty() {
        "/"
    } else {
        parsed.path()
    };
    dict.insert(
        Key::String("path".to_string()),
        ctx.alloc_thunk(ok_val(string_val(path), call_span)?),
    );

    // query (null if absent)
    let query = match parsed.query() {
        Some(q) => string_val(q),
        None => Value::Dict(IndexMap::new()),
    };
    dict.insert(
        Key::String("query".to_string()),
        ctx.alloc_thunk(ok_val(query, call_span)?),
    );

    // fragment (null if absent)
    let fragment = match parsed.fragment() {
        Some(f) => string_val(f),
        None => Value::Dict(IndexMap::new()),
    };
    dict.insert(
        Key::String("fragment".to_string()),
        ctx.alloc_thunk(ok_val(fragment, call_span)?),
    );

    ok_val(Value::Dict(dict), call_span)
}

/// Parse URN → Urn dict per RFC 8141
///
/// Returns: nid, nss, r-component, q-component, fragment.
/// Errors if scheme is not "urn".
pub(crate) fn builtin_urn(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    let val = expect_one_arg("urn", args, named, &ctx, depth, call_span)?;
    let s = match val {
        Value::String {
            ref source,
            start,
            end,
        } => &source[start..end],
        _ => {
            return Err(EvalError::type_mismatch("String", val.type_name(), call_span).into());
        }
    };

    // URN format: urn:NID:NSS[?+r-component][?=q-component][#fragment]
    let (scheme, rest) = match s.split_once(':') {
        Some((scheme, rest)) => (scheme, rest),
        None => {
            return Err(
                EvalError::uri_parse_error(format!("missing scheme: {}", s), call_span).into(),
            );
        }
    };

    if scheme.to_lowercase() != "urn" {
        return Err(EvalError::uri_parse_error(
            format!("expected URN scheme 'urn', got '{}'", scheme),
            call_span,
        )
        .into());
    }

    // Split off fragment first (#)
    let (main_part, fragment) = match rest.split_once('#') {
        Some((main, frag)) => (main, Some(frag)),
        None => (rest, None),
    };

    // Split off q-component (?=...)
    let (after_nss, q_component) = match main_part.split_once("?=") {
        Some((before, q)) => (before, Some(q)),
        None => (main_part, None),
    };

    // Split off r-component (?+...)
    let (nss_part, r_component) = match after_nss.split_once("?+") {
        Some((before, r)) => (before, Some(r)),
        None => (after_nss, None),
    };

    // Split NID and NSS (first colon after urn:)
    let (nid, nss) = match nss_part.split_once(':') {
        Some((nid, nss)) => (nid, nss),
        None => {
            return Err(EvalError::uri_parse_error(
                format!("URN missing NID:NSS separator: {}", s),
                call_span,
            )
            .into());
        }
    };

    let mut dict = IndexMap::new();

    dict.insert(
        Key::String("nid".to_string()),
        ctx.alloc_thunk(ok_val(string_val(nid), call_span)?),
    );
    dict.insert(
        Key::String("nss".to_string()),
        ctx.alloc_thunk(ok_val(string_val(nss), call_span)?),
    );

    // r-component (null if absent)
    let r_val = match r_component {
        Some(r) => string_val(r),
        None => Value::Dict(IndexMap::new()),
    };
    dict.insert(
        Key::String("r-component".to_string()),
        ctx.alloc_thunk(ok_val(r_val, call_span)?),
    );

    // q-component (null if absent)
    let q_val = match q_component {
        Some(q) => string_val(q),
        None => Value::Dict(IndexMap::new()),
    };
    dict.insert(
        Key::String("q-component".to_string()),
        ctx.alloc_thunk(ok_val(q_val, call_span)?),
    );

    // fragment (null if absent)
    let frag_val = match fragment {
        Some(f) => string_val(f),
        None => Value::Dict(IndexMap::new()),
    };
    dict.insert(
        Key::String("fragment".to_string()),
        ctx.alloc_thunk(ok_val(frag_val, call_span)?),
    );

    ok_val(Value::Dict(dict), call_span)
}
