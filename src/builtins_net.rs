//! Net builtin module — re-exports from builtins_io.rs (physical move deferred to a future sprint).
//!
//! This module provides:
//! - Re-exports of net builtin implementations from `builtins_io.rs`
//! - Re-exports of URI builtin implementations from `builtins_uri.rs`
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
//! All implementations live in `builtins_io.rs` (for net) and `builtins_uri.rs`
//! (for URI). The physical code move is deferred until a chunked migration tool
//! is available. The re-export approach here gives `builtins_net.rs` its own
//! stable identity for callers (builtins_core.rs, builtins.rs) without a 2000-line
//! copy operation.

// ── Re-exports from builtins_io.rs ────────────────────────────────────────────

pub(crate) use crate::builtins_io::{
    builtin_connect, builtin_http2_session, builtin_http3_session, builtin_http_request,
    builtin_icmp_ping, builtin_quic_open_datagram, builtin_quic_open_stream, builtin_quic_session,
    builtin_recv_datagram, builtin_send_datagram, builtin_tls_layer, builtin_tls_peer_cert,
};

// ── Re-exports from builtins_uri.rs ────────────────────────────────────────────
//
// T-761: physical move of builtins_uri.rs content into this file (and deletion of
// builtins_uri.rs) is deferred. Until then, we re-export the implementations here
// so that this module is the single import point for anything net-related.

pub(crate) use crate::builtins_uri::{builtin_uri, builtin_url, builtin_urn};

// ── Helpers re-exported for consumers that need them ──────────────────────────
//
// These are internal to the net subsystem; they are re-exported here so that
// builtins_net.rs is the single import point for anything net-related.

// net helpers are pub(crate) in builtins_io — no re-export needed here

// ── Registration ───────────────────────────────────────────────────────────────

use crate::builtins::builtin;
use crate::types::{Row, Type, TypeAlias, TypeEnv};
use crate::value::{BuiltinDef, Strictness};
use std::collections::HashMap;

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

    // ── tls-layer: Handle → String → Dict → Handle[... Tls] ──────────────────
    // Wraps an existing TCP Handle in TLS (STARTTLS pattern).
    env.insert(
        "builtin-tls-layer".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Handle(Box::new(cap_flag("readable")))),
                (None, Type::Str),
                (None, Type::Record(Row { fields: HashMap::new() })), // opts dict: no required fields (BAS width subtyping)
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

    // ── send-datagram: DatagramHandle → (String | Bytes) → [] ────────────────
    env.insert(
        "builtin-send-datagram".to_string(),
        Type::Function {
            params: vec![
                (
                    None,
                    Type::normalize_union(vec![Type::DatagramHandle, Type::QuicDatagramHandle]),
                ),
                (None, Type::normalize_union(vec![Type::Str, Type::Bytes])),
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
                (None, Type::Record(Row { fields: HashMap::new() })), // opts dict (TLS options; no required fields)
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
                (None, Type::Str),     // base_url
                (None, Type::Record(Row { fields: HashMap::new() })), // opts dict (reserved; no required fields)
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
                (None, Type::Str),     // method
                (None, Type::Str),     // path
                (None, Type::Record(Row { fields: HashMap::new() })), // headers dict (any dict; BAS width subtyping)
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
