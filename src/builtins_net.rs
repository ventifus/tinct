//! Net builtin module — re-exports from builtins_io.rs (physical move deferred to a future sprint).
//!
//! This module provides:
//! - Re-exports of net builtin implementations from `builtins_io.rs`
//! - URI builtin implementations: `uri`, `url`, `urn`
//! - `net_builtins()` — the registration list for the "net" module
//! - `net_type_env()` — type environment for all net/URI builtins
//!
//! **Net builtins covered:**
//! - `builtin-connect`: TCP/UDP/UNIX connection (NetCap-gated)
//! - `builtin-tls-layer`: Layer TLS on a connection
//! - `builtin-tls-peer-cert`: Extract TLS certificate metadata
//! - `builtin-send-datagram`: Send UDP/Unix datagram
//! - `builtin-recv-datagram`: Receive UDP/Unix datagram
//! - `quic-session`: Open a QUIC session (QUIC+TLS)
//! - `quic-open-stream`: Open a QUIC bidirectional stream
//! - `quic-open-datagram`: Open QUIC datagram channel
//! - `http2-session`: Open an HTTP/2 session
//! - `http3-session`: Open an HTTP/3 session (QUIC-based)
//! - `http-request`: Make an HTTP request over an existing session
//! - `icmp-ping`: Send ICMP echo request
//!
//! **URI builtins covered:**
//! - `uri`: Parse any URI string (RFC 3986)
//! - `url`: Parse a hierarchical URL (requires host)
//! - `urn`: Parse a URN (RFC 8141)
//!
//! ## Physical location
//!
//! Net builtin implementations live in `builtins_io.rs`.
//! URI builtin implementations live in this file.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::builtins::{builtin, expect_one_arg, ok_val};
use crate::error::{EvalError, EvalResult};
use crate::types::{Row, Type, TypeAlias, TypeEnv};
use crate::value::{string_val, BuiltinArgs, BuiltinDef, Key, Strictness, Thunk, Value};
use std::collections::HashMap;

// ── Re-exports from builtins_io.rs ────────────────────────────────────────────

pub(crate) use crate::builtins_io::{
    builtin_connect, builtin_http2_session, builtin_http3_session, builtin_http_request,
    builtin_icmp_ping, builtin_quic_open_datagram, builtin_quic_open_stream, builtin_quic_session,
    builtin_recv_datagram, builtin_send_datagram, builtin_tls_layer, builtin_tls_peer_cert,
};

// ── URI parsing builtins ───────────────────────────────────────────────────────

/// Parse any URI string → Uri dict
///
/// Returns a Dict with: scheme, username, password, host, port, path, query, fragment.
/// host/port are null for non-hierarchical URIs (mailto:, tel:, urn:, news:).
/// username/password extracted by splitting userinfo on ":" (RFC 3986 convention).
pub(crate) fn builtin_uri(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        let val = expect_one_arg("uri", &args, named.as_ref(), &ctx, call_span.clone())?;
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
                Key::String("scheme".into()),
                ctx.alloc_thunk(ok_val(string_val(parsed.scheme()), call_span.clone())?),
            );

            // username (split from userinfo)
            let username = if parsed.username().is_empty() {
                Value::Dict(IndexMap::new())
            } else {
                string_val(parsed.username())
            };
            dict.insert(
                Key::String("username".into()),
                ctx.alloc_thunk(ok_val(username, call_span.clone())?),
            );

            // password (split from userinfo)
            let password = match parsed.password() {
                Some(pw) => string_val(pw),
                None => Value::Dict(IndexMap::new()),
            };
            dict.insert(
                Key::String("password".into()),
                ctx.alloc_thunk(ok_val(password, call_span.clone())?),
            );

            // host (null for non-hierarchical; strip IPv6 brackets)
            let host = match parsed.host_str() {
                Some(h) => string_val(h),
                None => Value::Dict(IndexMap::new()),
            };
            dict.insert(
                Key::String("host".into()),
                ctx.alloc_thunk(ok_val(host, call_span.clone())?),
            );

            // port (null if not specified)
            let port = match parsed.port() {
                Some(p) => Value::Int(i64::from(p)),
                None => Value::Dict(IndexMap::new()),
            };
            dict.insert(
                Key::String("port".into()),
                ctx.alloc_thunk(ok_val(port, call_span.clone())?),
            );

            // path (always present per RFC 3986)
            dict.insert(
                Key::String("path".into()),
                ctx.alloc_thunk(ok_val(string_val(parsed.path()), call_span.clone())?),
            );

            // query (null if absent)
            let query = match parsed.query() {
                Some(q) => string_val(q),
                None => Value::Dict(IndexMap::new()),
            };
            dict.insert(
                Key::String("query".into()),
                ctx.alloc_thunk(ok_val(query, call_span.clone())?),
            );

            // fragment (null if absent)
            let fragment = match parsed.fragment() {
                Some(f) => string_val(f),
                None => Value::Dict(IndexMap::new()),
            };
            dict.insert(
                Key::String("fragment".into()),
                ctx.alloc_thunk(ok_val(fragment, call_span.clone())?),
            );

            return ok_val(Value::Dict(dict), call_span);
        }

        // Fallback: manual parsing for non-hierarchical URIs (mailto:, tel:, urn:, news:)
        // These don't have authority (host/port), so url::Url rejects them.
        let (scheme, rest) = match s.split_once(':') {
            Some((scheme, rest)) => (scheme, rest),
            None => {
                return Err(EvalError::uri_parse_error(
                    format!("missing scheme: {}", s),
                    call_span,
                )
                .into());
            }
        };

        let mut dict = IndexMap::new();

        dict.insert(
            Key::String("scheme".into()),
            ctx.alloc_thunk(ok_val(
                string_val(&scheme.to_lowercase()),
                call_span.clone(),
            )?),
        );

        // Non-hierarchical URIs: all null for userinfo/host/port
        for key in ["username", "password", "host", "port"] {
            dict.insert(
                Key::String(key.into()),
                ctx.alloc_thunk(ok_val(Value::Dict(IndexMap::new()), call_span.clone())?),
            );
        }

        // path is the remaining part after scheme:
        // For mailto:user@example.com, path is "user@example.com"
        // For urn:isbn:123, path is "isbn:123"
        dict.insert(
            Key::String("path".into()),
            ctx.alloc_thunk(ok_val(string_val(rest), call_span.clone())?),
        );

        // query and fragment: null (non-hierarchical URIs typically don't have these)
        for key in ["query", "fragment"] {
            dict.insert(
                Key::String(key.into()),
                ctx.alloc_thunk(ok_val(Value::Dict(IndexMap::new()), call_span.clone())?),
            );
        }

        ok_val(Value::Dict(dict), call_span)
    })
}

/// Parse hierarchical URL → Url dict
///
/// Errors if no authority (no host). Port defaults to scheme default if not specified.
pub(crate) fn builtin_url(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        let val = expect_one_arg("url", &args, named.as_ref(), &ctx, call_span.clone())?;
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

        let parsed = url::Url::parse(s).map_err(|e| {
            EvalError::uri_parse_error(format!("invalid URL: {}", e), call_span.clone())
        })?;

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
            Key::String("scheme".into()),
            ctx.alloc_thunk(ok_val(string_val(parsed.scheme()), call_span.clone())?),
        );

        // username (split from userinfo)
        let username = if parsed.username().is_empty() {
            Value::Dict(IndexMap::new())
        } else {
            string_val(parsed.username())
        };
        dict.insert(
            Key::String("username".into()),
            ctx.alloc_thunk(ok_val(username, call_span.clone())?),
        );

        // password (split from userinfo)
        let password = match parsed.password() {
            Some(pw) => string_val(pw),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            Key::String("password".into()),
            ctx.alloc_thunk(ok_val(password, call_span.clone())?),
        );

        // host (always present for URLs; unwrap is safe)
        dict.insert(
            Key::String("host".into()),
            ctx.alloc_thunk(ok_val(
                string_val(parsed.host_str().unwrap()),
                call_span.clone(),
            )?),
        );

        // port (default to scheme default if not specified)
        let port = parsed.port_or_known_default().unwrap_or({
            // Fallback for unknown schemes: return port 0 as sentinel
            // (url::Url::port_or_known_default returns None for unknown schemes)
            0
        });
        dict.insert(
            Key::String("port".into()),
            ctx.alloc_thunk(ok_val(Value::Int(i64::from(port)), call_span.clone())?),
        );

        // path (always present per RFC 3986; default to "/" if empty)
        let path = if parsed.path().is_empty() {
            "/"
        } else {
            parsed.path()
        };
        dict.insert(
            Key::String("path".into()),
            ctx.alloc_thunk(ok_val(string_val(path), call_span.clone())?),
        );

        // query (null if absent)
        let query = match parsed.query() {
            Some(q) => string_val(q),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            Key::String("query".into()),
            ctx.alloc_thunk(ok_val(query, call_span.clone())?),
        );

        // fragment (null if absent)
        let fragment = match parsed.fragment() {
            Some(f) => string_val(f),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            Key::String("fragment".into()),
            ctx.alloc_thunk(ok_val(fragment, call_span.clone())?),
        );

        ok_val(Value::Dict(dict), call_span)
    })
}

/// Parse URN → Urn dict per RFC 8141
///
/// Returns: nid, nss, r-component, q-component, fragment.
/// Errors if scheme is not "urn".
pub(crate) fn builtin_urn(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        let val = expect_one_arg("urn", &args, named.as_ref(), &ctx, call_span.clone())?;
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
                return Err(EvalError::uri_parse_error(
                    format!("missing scheme: {}", s),
                    call_span,
                )
                .into());
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
            Key::String("nid".into()),
            ctx.alloc_thunk(ok_val(string_val(nid), call_span.clone())?),
        );
        dict.insert(
            Key::String("nss".into()),
            ctx.alloc_thunk(ok_val(string_val(nss), call_span.clone())?),
        );

        // r-component (null if absent)
        let r_val = match r_component {
            Some(r) => string_val(r),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            Key::String("r-component".into()),
            ctx.alloc_thunk(ok_val(r_val, call_span.clone())?),
        );

        // q-component (null if absent)
        let q_val = match q_component {
            Some(q) => string_val(q),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            Key::String("q-component".into()),
            ctx.alloc_thunk(ok_val(q_val, call_span.clone())?),
        );

        // fragment (null if absent)
        let frag_val = match fragment {
            Some(f) => string_val(f),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            Key::String("fragment".into()),
            ctx.alloc_thunk(ok_val(frag_val, call_span.clone())?),
        );

        ok_val(Value::Dict(dict), call_span)
    })
}

// ── Registration ───────────────────────────────────────────────────────────────

/// Return the builtin registration list for the "net" module.
///
/// This covers all network and URI builtins. Called by `builtin_module("net")`
/// in `src/builtins.rs` (wired in T-718/T-719).
pub fn net_builtins() -> Vec<BuiltinDef> {
    vec![
        // ── TCP/UDP/UNIX connections ─────────────────────────────────────────
        builtin!(
            "builtin-connect",
            builtin_connect,
            [
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq
            ]
        ),
        builtin!(
            "builtin-tls-layer",
            builtin_tls_layer,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3
        ),
        builtin!(
            "builtin-tls-peer-cert",
            builtin_tls_peer_cert,
            [Strictness::Seq]
        ),
        // ── Datagram sockets (UDP, Unix datagram) ────────────────────────────
        builtin!(
            "builtin-send-datagram",
            builtin_send_datagram,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-recv-datagram",
            builtin_recv_datagram,
            [Strictness::Seq]
        ),
        // ── QUIC ─────────────────────────────────────────────────────────────
        builtin!(
            "quic-session",
            builtin_quic_session,
            [
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq
            ],
            4
        ),
        builtin!(
            "quic-open-stream",
            builtin_quic_open_stream,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "quic-open-datagram",
            builtin_quic_open_datagram,
            [Strictness::Seq],
            1
        ),
        // ── HTTP ─────────────────────────────────────────────────────────────
        builtin!(
            "http2-session",
            builtin_http2_session,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3
        ),
        builtin!("http3-session", builtin_http3_session, [Strictness::Seq], 1),
        builtin!(
            "http-request",
            builtin_http_request,
            [
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq
            ],
            5
        ),
        // ── ICMP ─────────────────────────────────────────────────────────────
        builtin!(
            "icmp-ping",
            builtin_icmp_ping,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3
        ),
        // ── URI parsing ───────────────────────────────────────────────────────
        builtin!("uri", builtin_uri, [Strictness::Seq]),
        builtin!("url", builtin_url, [Strictness::Seq]),
        builtin!("urn", builtin_urn, [Strictness::Seq]),
    ]
}

/// Return a `TypeEnv` with type signatures for all "net" module builtins.
///
/// Covers all builtins listed in `net_builtins()` plus the associated type aliases:
/// `QuicSession`, `Http2Session`, `Http3Session`, `QuicDatagramHandle`, `DatagramHandle`,
/// `Url`. Called by `type_env_module("net")` in `src/builtins.rs`.
pub fn net_type_env() -> TypeEnv {
    let mut env = TypeEnv::new();
    populate_net_type_env(&mut env);
    env
}

/// Populate `env` with type signatures for all "net" module builtins and type aliases.
pub fn populate_net_type_env(env: &mut TypeEnv) {
    // Helper: create Handle capability flag type (Readable, Writable, etc.)
    fn cap_flag(flag_name: &str) -> Type {
        let mut fields = HashMap::new();
        fields.insert(
            format!("__cap_flag_{}", flag_name.to_lowercase()),
            Type::Record(Row {
                fields: HashMap::new(),
            }),
        );
        Type::Record(Row { fields })
    }

    // ── Type aliases ──────────────────────────────────────────────────────────
    // Register so @QuicSession, @Http2Session, etc. are valid in user annotations.

    env.insert_type_alias(
        "QuicSession".to_string(),
        TypeAlias {
            params: vec![],
            body: Type::QuicSession,
        },
    );
    env.insert_type_alias(
        "Http2Session".to_string(),
        TypeAlias {
            params: vec![],
            body: Type::Http2Session,
        },
    );
    env.insert_type_alias(
        "Http3Session".to_string(),
        TypeAlias {
            params: vec![],
            body: Type::Http3Session,
        },
    );
    env.insert_type_alias(
        "QuicDatagramHandle".to_string(),
        TypeAlias {
            params: vec![],
            body: Type::QuicDatagramHandle,
        },
    );
    env.insert_type_alias(
        "DatagramHandle".to_string(),
        TypeAlias {
            params: vec![],
            body: Type::DatagramHandle,
        },
    );
    // Url — type alias for Uri (url/urn builtins also return Uri)
    env.insert_type_alias(
        "Url".to_string(),
        TypeAlias {
            params: vec![],
            body: Type::Uri,
        },
    );

    // ── connect: NetCap → String → Int → String → Handle[Readable Writable Binary Stream] ──
    // Takes (cap, host, port, transport-tag). Returns a bidirectional stream handle.
    env.insert(
        "builtin-connect".to_string(),
        Type::Function {
            params: vec![
                (None, Type::NetCap),
                (None, Type::Str),
                (None, Type::Int),
                (None, Type::Str),
            ],
            ret: Box::new(Type::Handle(Box::new(cap_flag("readable")))),
            variadic: false,
        },
    );

    // ── tls-layer: String → Dict → Handle → Handle[... Tls] ──────────────────
    // Wraps an existing TCP Handle in TLS (STARTTLS pattern).
    env.insert(
        "builtin-tls-layer".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Str), // sni
                (
                    None,
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                ), // opts dict: no required fields (BAS width subtyping)
                (None, Type::Handle(Box::new(cap_flag("readable")))), // handle
            ],
            ret: Box::new(Type::Handle(Box::new(cap_flag("readable")))),
            variadic: false,
        },
    );

    // ── tls-peer-cert: Handle → Dict ─────────────────────────────────────────
    // Extracts TLS certificate metadata from a TLS handle.
    env.insert(
        "builtin-tls-peer-cert".to_string(),
        Type::Function {
            params: vec![(None, Type::Handle(Box::new(cap_flag("readable"))))],
            ret: Box::new(Type::Unknown), // {subject, issuer, sans, not-before, not-after, spki-sha256}
            variadic: false,
        },
    );

    // ── send-datagram: (String | Bytes) → DatagramHandle → [] ────────────────
    env.insert(
        "builtin-send-datagram".to_string(),
        Type::Function {
            params: vec![
                (None, Type::normalize_union(vec![Type::Str, Type::Bytes])),
                (
                    None,
                    Type::normalize_union(vec![Type::DatagramHandle, Type::QuicDatagramHandle]),
                ),
            ],
            ret: Box::new(Type::Record(Row {
                fields: HashMap::new(),
            })),
            variadic: false,
        },
    );

    // ── recv-datagram: DatagramHandle → {data: String} ───────────────────────
    env.insert(
        "builtin-recv-datagram".to_string(),
        Type::Function {
            params: vec![(
                None,
                Type::normalize_union(vec![Type::DatagramHandle, Type::QuicDatagramHandle]),
            )],
            ret: Box::new(Type::Unknown), // {data: String}
            variadic: false,
        },
    );

    // ── quic-session: NetCap → String → Int → Dict → QuicSession ─────────────
    env.insert(
        "quic-session".to_string(),
        Type::Function {
            params: vec![
                (None, Type::NetCap),
                (None, Type::Str),
                (None, Type::Int),
                (
                    None,
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                ), // opts dict (TLS options; no required fields)
            ],
            ret: Box::new(Type::QuicSession),
            variadic: false,
        },
    );

    // ── quic-open-stream: QuicSession → Handle[Readable Writable Binary Stream] ──
    env.insert(
        "quic-open-stream".to_string(),
        Type::Function {
            params: vec![(None, Type::QuicSession)],
            ret: Box::new(Type::Handle(Box::new(cap_flag("readable")))),
            variadic: false,
        },
    );

    // ── quic-open-datagram: QuicSession → QuicDatagramHandle ─────────────────
    env.insert(
        "quic-open-datagram".to_string(),
        Type::Function {
            params: vec![(None, Type::QuicSession)],
            ret: Box::new(Type::QuicDatagramHandle),
            variadic: false,
        },
    );

    // ── http2-session: NetCap → String → Dict → Http2Session ─────────────────
    env.insert(
        "http2-session".to_string(),
        Type::Function {
            params: vec![
                (None, Type::NetCap),
                (None, Type::Str), // base_url
                (
                    None,
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                ), // opts dict (reserved; no required fields)
            ],
            ret: Box::new(Type::Http2Session),
            variadic: false,
        },
    );

    // ── http3-session: QuicSession → Http3Session ─────────────────────────────
    env.insert(
        "http3-session".to_string(),
        Type::Function {
            params: vec![(None, Type::QuicSession)],
            ret: Box::new(Type::Http3Session),
            variadic: false,
        },
    );

    // ── http-request: (Http2Session | Http3Session) → String → String → Dict → String → Dict ──
    // Returns {status: Int, headers: Dict, body: String} on success (or error via builtin-try).
    env.insert(
        "http-request".to_string(),
        Type::Function {
            params: vec![
                (
                    None,
                    Type::normalize_union(vec![Type::Http2Session, Type::Http3Session]),
                ),
                (None, Type::Str), // method
                (None, Type::Str), // path
                (
                    None,
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                ), // headers dict (any dict; BAS width subtyping)
                (None, Type::Str), // body: runtime calls require_string — Bytes not accepted
            ],
            ret: Box::new(Type::Unknown), // {status: Int, headers: Dict, body: String}
            variadic: false,
        },
    );

    // ── icmp-ping: NetCap → String → Int → Dict ───────────────────────────────
    // Returns {ok: {latency-ms: Int}} or {err: String} via builtin-try.
    env.insert(
        "icmp-ping".to_string(),
        Type::Function {
            params: vec![
                (None, Type::NetCap),
                (None, Type::Str), // host
                (None, Type::Int), // timeout_ms
            ],
            ret: Box::new(Type::Unknown), // {ok: {latency-ms: Int}} | {err: String}
            variadic: false,
        },
    );

    // ── uri: String → Uri ─────────────────────────────────────────────────────
    env.insert(
        "uri".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Uri),
            variadic: false,
        },
    );

    // ── url: String → Uri ─────────────────────────────────────────────────────
    // (hierarchical URL — requires host; returns same Uri type)
    env.insert(
        "url".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Uri),
            variadic: false,
        },
    );

    // ── urn: String → Uri ─────────────────────────────────────────────────────
    env.insert(
        "urn".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Uri),
            variadic: false,
        },
    );
}
