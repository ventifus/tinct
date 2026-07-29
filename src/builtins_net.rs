//! Net builtin module — network I/O and URI parsing.
//!
//! This module provides:
//! - Network builtin implementations: TLS, QUIC, HTTP, ICMP
//! - URI builtin implementations: `uri`, `url`, `urn`
//! - `net_builtins()` — the registration list for the "net" module
//! - `net_type_env()` — type environment for all net/URI builtins
//!
//! **Net builtins covered:**
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
//! Extracted from `builtins_io.rs` in T-915.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::{builtin, expect_one_arg, ok_val, reject_named, require_string};

use crate::error::{EvalError, EvalResult};
use crate::value::{string_val, BuiltinArgs, BuiltinDef, HashableValue, Strictness, Thunk, Value};

/// Guards rustls crypto provider initialization. `get_or_init` ensures
/// `install_default()` is called exactly once per process lifetime, making
/// the "already installed" `Err` case structurally impossible.
static RUSTLS_CRYPTO_INIT: OnceLock<()> = OnceLock::new();

/// Check if a connection to host:port is allowed by the NetCap allowlist.
/// Returns Ok(None) for hostname-only match, Ok(Some(ip)) for IP-based match requiring DNS resolution.
/// For host-only transports (ICMP), pass port=None — HostPort entries won't match, but Hostname/Glob/CIDR will.
pub(crate) fn check_net_cap_allowlist(
    entries: &[crate::value::NetCapEntry],
    host: &str,
    port: Option<u16>,
    span: Span,
) -> EvalResult<Option<std::net::IpAddr>> {
    use crate::value::NetCapEntry;
    use std::net::IpAddr;

    // Quick check: Any entry allows everything
    if entries.iter().any(|e| matches!(e, NetCapEntry::Any)) {
        return Ok(None);
    }

    // Try to parse host as IP address — a parse failure means host is a hostname, not an IP literal.
    let host_ip: Option<IpAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => Some(ip),
        Err(_) => None, // host is a hostname, not an IP literal — expected non-error condition
    };

    // If host is an IP literal, check CIDR entries
    if let Some(ip) = host_ip {
        for entry in entries {
            if let NetCapEntry::Cidr(net) = entry {
                if net.contains(&ip) {
                    return Ok(None); // Direct IP match, no DNS needed
                }
            }
        }
        // IP literal not in any CIDR — deny
        return Err(EvalError::user_error(
            format!("connect: IP address {} not in any allowed CIDR range", host),
            span,
        )
        .into());
    }

    // Host is a hostname — check hostname-based entries first
    let mut hostname_match = false;
    for entry in entries {
        match entry {
            NetCapEntry::Hostname(allowed_host) if host.eq_ignore_ascii_case(allowed_host) => {
                hostname_match = true;
                break;
            }
            NetCapEntry::HostPort(allowed_host, allowed_port) => {
                if let Some(p) = port {
                    if host.eq_ignore_ascii_case(allowed_host) && p == *allowed_port {
                        hostname_match = true;
                        break;
                    }
                }
                // If port is None (ICMP, etc.), HostPort entries don't match
            }
            NetCapEntry::HostnameGlob(pattern) => {
                // Pattern: "*.suffix"
                if let Some(suffix) = pattern.strip_prefix("*.") {
                    if host.eq_ignore_ascii_case(suffix) || host.ends_with(&format!(".{}", suffix))
                    {
                        hostname_match = true;
                        break;
                    }
                }
            }
            _ => {}
        }
    }

    // Check if any CIDR entries exist
    let has_cidr = entries.iter().any(|e| matches!(e, NetCapEntry::Cidr(_)));

    if hostname_match && !has_cidr {
        // Hostname match, no CIDR restrictions — allow without DNS resolution
        return Ok(None);
    }

    if hostname_match && has_cidr {
        // Hostname match, but CIDR entries exist — must resolve and validate IP
        // This prevents hostname-only match from bypassing IP-based restrictions
        let resolved_ip = resolve_hostname_for_cidr(host, entries, span)?;
        return Ok(Some(resolved_ip));
    }

    if !hostname_match && has_cidr {
        // No hostname match, but CIDR entries exist — try DNS resolution
        let resolved_ip = resolve_hostname_for_cidr(host, entries, span)?;
        return Ok(Some(resolved_ip));
    }

    // No match at all — deny
    let target = if let Some(p) = port {
        format!("{}:{}", host, p)
    } else {
        host.to_string()
    };
    Err(EvalError::user_error(
        format!(
            "connect: connection to {} denied by NetCap allowlist",
            target
        ),
        span,
    )
    .into())
}

/// Resolve hostname to IP and validate against CIDR entries.
/// Returns the first IP that matches a CIDR entry.
fn resolve_hostname_for_cidr(
    host: &str,
    entries: &[crate::value::NetCapEntry],
    span: Span,
) -> EvalResult<std::net::IpAddr> {
    use crate::value::NetCapEntry;
    use std::net::ToSocketAddrs;

    // Resolve hostname to IP addresses
    let dummy_port = 0; // ToSocketAddrs requires a port, but we don't use it
    let addrs: Vec<std::net::IpAddr> = match (host, dummy_port).to_socket_addrs() {
        Ok(iter) => iter.map(|sa| sa.ip()).collect(),
        Err(e) => {
            return Err(EvalError::user_error(
                format!("connect: failed to resolve hostname '{}': {}", host, e),
                span,
            )
            .into())
        }
    };

    if addrs.is_empty() {
        return Err(EvalError::user_error(
            format!("connect: no IP addresses found for hostname '{}'", host),
            span,
        )
        .into());
    }

    // Check each resolved IP against CIDR entries
    for ip in &addrs {
        for entry in entries {
            if let NetCapEntry::Cidr(net) = entry {
                if net.contains(ip) {
                    return Ok(*ip); // Found a match
                }
            }
        }
    }

    // No resolved IP matched any CIDR
    Err(EvalError::user_error(
        format!(
            "connect: resolved IPs for '{}' ({:?}) not in any allowed CIDR range",
            host, addrs
        ),
        span,
    )
    .into())
}

// ============================================================================
// TLS Support
// ============================================================================

/// Build a rustls ClientConfig from the opts dict
pub(crate) async fn build_tls_config(
    opts_val: &Value,
    opts_span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<rustls::ClientConfig> {
    use rustls::RootCertStore;

    // Install the ring crypto provider exactly once per process lifetime.
    // rustls 0.23 requires an explicit provider; ring is the default for tinct.
    // RUSTLS_CRYPTO_INIT ensures install_default() is called at most once,
    // making the "already installed" Err case structurally impossible.
    RUSTLS_CRYPTO_INIT.get_or_init(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("rustls ring provider installation failed");
    });

    let opts_dict = match opts_val {
        Value::Dict { entries: d, .. } => d,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "tls-connect opts".to_string(),
                "Dict",
                other.type_name(),
                opts_span,
            )
            .into())
        }
    };

    let mut root_store = RootCertStore::empty();

    // Check no-system-roots
    let no_system_roots = if let Some(thunk) =
        opts_dict.get(&crate::value::HashableValue::Str("no-system-roots".into()))
    {
        let val = crate::eval::materialize(thunk, Some(&opts_span), ctx).await?;
        matches!(val, crate::value::Value::Int { n, .. } if n != 0)
    } else {
        false
    };

    // Load system roots unless disabled
    if !no_system_roots {
        let cert_result = rustls_native_certs::load_native_certs();

        // Report any errors encountered while loading certs
        if !cert_result.errors.is_empty() {
            // Collect error messages
            let error_msgs: Vec<String> =
                cert_result.errors.iter().map(|e| e.to_string()).collect();
            return Err(EvalError::user_error(
                format!(
                    "tls-connect: failed to load system CA roots: {}",
                    error_msgs.join("; ")
                ),
                opts_span.clone(),
            )
            .into());
        }

        for cert in cert_result.certs {
            root_store.add(cert).map_err(|e| {
                EvalError::user_error(
                    format!("tls-connect: failed to add system CA cert: {}", e),
                    opts_span.clone(),
                )
            })?;
        }
    }

    // Load mozilla-roots if requested
    let mozilla_roots = if let Some(thunk) =
        opts_dict.get(&crate::value::HashableValue::Str("mozilla-roots".into()))
    {
        let val = crate::eval::materialize(thunk, Some(&opts_span), ctx).await?;
        matches!(val, crate::value::Value::Int { n, .. } if n != 0)
    } else {
        false
    };

    if mozilla_roots {
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }

    // Load ca-bundle if provided
    if let Some(thunk) = opts_dict.get(&crate::value::HashableValue::Str("ca-bundle".into())) {
        let handle_val = crate::eval::materialize(thunk, Some(&opts_span), ctx).await?;
        let pem_bytes = slurp_handle_bytes(&handle_val, opts_span.clone())?;

        let mut cursor = std::io::Cursor::new(pem_bytes);
        let certs = rustls_pemfile::certs(&mut cursor)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                EvalError::user_error(
                    format!("tls-connect: failed to parse CA bundle PEM: {}", e),
                    opts_span.clone(),
                )
            })?;

        for cert in certs {
            root_store.add(cert).map_err(|e| {
                EvalError::user_error(
                    format!("tls-connect: failed to add CA bundle cert: {}", e),
                    opts_span.clone(),
                )
            })?;
        }
    }

    // Build config with client auth
    let has_client_cert =
        opts_dict.contains_key(&crate::value::HashableValue::Str("client-cert".into()));
    let has_client_key =
        opts_dict.contains_key(&crate::value::HashableValue::Str("client-key".into()));

    let mut config = if has_client_cert || has_client_key {
        if !has_client_cert || !has_client_key {
            return Err(EvalError::user_error(
                "tls-connect: both client-cert and client-key must be provided for mTLS"
                    .to_string(),
                opts_span.clone(),
            )
            .into());
        }

        let cert_thunk = opts_dict
            .get(&crate::value::HashableValue::Str("client-cert".into()))
            .unwrap();
        let cert_handle = crate::eval::materialize(cert_thunk, Some(&opts_span), ctx).await?;

        let key_thunk = opts_dict
            .get(&crate::value::HashableValue::Str("client-key".into()))
            .unwrap();
        let key_handle = crate::eval::materialize(key_thunk, Some(&opts_span), ctx).await?;

        let cert_pem = slurp_handle_bytes(&cert_handle, opts_span.clone())?;
        let key_pem = slurp_handle_bytes(&key_handle, opts_span.clone())?;

        let mut cert_cursor = std::io::Cursor::new(cert_pem);
        let certs = rustls_pemfile::certs(&mut cert_cursor)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                EvalError::user_error(
                    format!("tls-connect: failed to parse client cert PEM: {}", e),
                    opts_span.clone(),
                )
            })?;

        let mut key_cursor = std::io::Cursor::new(key_pem);
        let key = rustls_pemfile::private_key(&mut key_cursor)
            .map_err(|e| {
                EvalError::user_error(
                    format!("tls-connect: failed to parse client key PEM: {}", e),
                    opts_span.clone(),
                )
            })?
            .ok_or_else(|| {
                EvalError::user_error(
                    "tls-connect: no private key found in client-key PEM".to_string(),
                    opts_span.clone(),
                )
            })?;

        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_client_auth_cert(certs, key)
            .map_err(|e| {
                EvalError::user_error(
                    format!("tls-connect: failed to configure client certificate: {}", e),
                    opts_span.clone(),
                )
            })?
    } else {
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth()
    };

    // Set ALPN protocols
    if let Some(thunk) = opts_dict.get(&crate::value::HashableValue::Str("alpn".into())) {
        let alpn_val = crate::eval::materialize(thunk, Some(&opts_span), ctx).await?;
        let alpn_protocols = extract_alpn_protocols(&alpn_val, opts_span, ctx).await?;
        config.alpn_protocols = alpn_protocols;
    } else {
        // Default ALPN: http/1.1
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
    }

    Ok(config)
}

/// Extract ALPN protocol list from a Dict of Strings (integer-keyed).
async fn extract_alpn_protocols(
    val: &Value,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Vec<Vec<u8>>> {
    let map = match val {
        Value::Dict { entries: d, .. } => d,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "tls-connect opts.alpn".to_string(),
                "Dict of String",
                other.type_name(),
                span,
            )
            .into())
        }
    };
    let mut protocols = Vec::new();
    for (_idx, thunk) in map {
        let v = crate::eval::materialize(thunk, Some(&span), ctx).await?;
        let protocol_str = match v {
            Value::String {
                source, start, end, ..
            } => source[start..end].to_string(),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "tls-connect opts.alpn".to_string(),
                    "String",
                    other.type_name(),
                    span,
                )
                .into())
            }
        };
        protocols.push(protocol_str.into_bytes());
    }
    Ok(protocols)
}

/// Slurp a Handle into bytes (for reading PEM files)
fn slurp_handle_bytes(_val: &Value, span: Span) -> EvalResult<Vec<u8>> {
    // Handle removed — this function can no longer read PEM data from a Handle.
    // When the network layer is redesigned, this will accept a File or Bytes value instead.
    Err(EvalError::user_error(
        "tls-connect: ca-bundle/client-cert/client-key via Handle not available — tcp redesign in progress".to_string(),
        span,
    )
    .into())
}

/// `tls-layer`: Layer TLS on an existing TCP Handle (STARTTLS use case).
/// Stubbed: Handle/WriteHandle removed. Will be reimplemented with new stream type.
pub(crate) fn builtin_tls_layer(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs { call_span, .. } = ctx_arg;
    Box::pin(async move {
        Err(EvalError::user_error(
            "tls-layer: network not yet available — tcp redesign in progress".to_string(),
            call_span,
        )
        .into())
    })
}

// Old tls-layer body removed (used Value::Handle). Network redesign sprint will rewrite.

// FINAL DEAD CODE REMOVAL: Lines from here to the tls-peer-cert doc are dead.
// They reference Value::Handle which no longer exists.
// The stub builtin_tls_layer above returns an error.

// UNIQUE_SENTINEL_TLS_LAYER_END

// DEAD CODE: TLS stream setup removed (used Value::Handle)

// tls-layer dead body fully removed.

/// `tls-peer-cert`: Extract TLS certificate metadata. Stubbed: Handle removed.
pub(crate) fn builtin_tls_peer_cert(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs { call_span, .. } = ctx_arg;
    Box::pin(async move {
        Err(EvalError::user_error(
            "tls-peer-cert: network not yet available — tcp redesign in progress".to_string(),
            call_span,
        )
        .into())
    })
}

// Old tls-peer-cert body fully removed (it used Value::Handle and async/? in non-async context).

// ── HTTP-sessions: QUIC and HTTP/3 ──────────────────────────────────────────────

/// `quic-session`: Open a QUIC connection to a remote host.
///
/// Takes `(cap, host, port, opts)` where:
/// - `cap`  — a NetCap allowing the target host/port
/// - `host` — hostname or IP string
/// - `port` — integer port (1–65535)
/// - `opts` — TLS options dict (same keys as `tls-connect`: `no-system-roots`,
///   `mozilla-roots`, `ca-bundle`, `client-cert`, `client-key`, `alpn`, `pins`)
///
/// Returns a `QuicSession` on success.
pub(crate) fn builtin_quic_session(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        use std::net::{SocketAddr, ToSocketAddrs};
        use std::sync::Arc;

        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        reject_named("quic-session", named.as_ref(), call_span.clone())?;

        if args.len() != 4 {
            return Err(EvalError::user_error(
                format!(
                    "quic-session: expected 4 arguments (cap host port opts), got {}",
                    args.len()
                ),
                call_span,
            )
            .into());
        }

        // All args pre-materialized by force_count
        let cap_val = Arc::clone(&args[0]).require_value()?.clone();
        let host_val = Arc::clone(&args[1]).require_value()?.clone();
        let port_val = Arc::clone(&args[2]).require_value()?.clone();
        let opts_val = Arc::clone(&args[3]).require_value()?.clone();

        // Extract NetCap
        let entries = match cap_val {
            Value::NetCap { entries: e, .. } => e,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "quic-session".to_string(),
                    "NetCap",
                    other.type_name(),
                    call_span.clone(),
                )
                .into())
            }
        };

        let host_str = require_string("quic-session", host_val, call_span.clone())?;

        let port = match port_val {
            Value::Int { n, .. } if (1..=65535).contains(&n) => n as u16,
            Value::Int { .. } => {
                return Err(EvalError::user_error(
                    "quic-session: port must be 1–65535".to_string(),
                    call_span.clone(),
                )
                .into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "quic-session".to_string(),
                    "Int",
                    other.type_name(),
                    call_span.clone(),
                )
                .into())
            }
        };

        // Validate against NetCap allowlist (DNS-rebinding mitigation)
        let resolved_ip =
            check_net_cap_allowlist(&entries, &host_str, Some(port), call_span.clone())?;

        // Determine server address for connection
        let server_addr: SocketAddr = if let Some(ip) = resolved_ip {
            SocketAddr::new(ip, port)
        } else {
            // Resolve hostname
            format!("{}:{}", host_str, port)
                .to_socket_addrs()
                .map_err(|e| {
                    EvalError::user_error(
                        format!("quic-session: failed to resolve '{}': {}", host_str, e),
                        call_span.clone(),
                    )
                })?
                .next()
                .ok_or_else(|| {
                    EvalError::user_error(
                        format!("quic-session: no addresses for '{}'", host_str),
                        call_span.clone(),
                    )
                })?
        };

        // Build rustls ClientConfig, then adapt it for QUIC via quinn's rustls adapter.
        // ALPN defaults to "h3" for QUIC sessions (RFC 9114 §3.1).
        let mut tls_config = build_tls_config(&opts_val, call_span.clone(), &ctx).await?;

        // Override ALPN to h3 unless caller specified explicit alpn in opts.
        // build_tls_config sets alpn_protocols to ["http/1.1"] by default; replace with h3.
        // We check opts for an explicit alpn key to respect caller overrides.
        let has_explicit_alpn = matches!(&opts_val, Value::Dict { entries: d, .. }
        if d.contains_key(&crate::value::HashableValue::Str("alpn".into())));
        if !has_explicit_alpn {
            tls_config.alpn_protocols = vec![b"h3".to_vec()];
        }

        // Adapt rustls config for QUIC
        let quic_tls =
            quinn::crypto::rustls::QuicClientConfig::try_from(tls_config).map_err(|e| {
                EvalError::user_error(
                    format!("quic-session: TLS config not suitable for QUIC: {}", e),
                    call_span.clone(),
                )
            })?;

        let client_config = quinn::ClientConfig::new(Arc::new(quic_tls));

        // Create a client endpoint bound to an ephemeral local UDP port
        let bind_addr: SocketAddr = "0.0.0.0:0".parse().expect("valid bind addr");
        let mut endpoint = quinn::Endpoint::client(bind_addr).map_err(|e| {
            EvalError::user_error(
                format!("quic-session: failed to create QUIC endpoint: {}", e),
                call_span.clone(),
            )
        })?;
        endpoint.set_default_client_config(client_config);

        // Connect (async → sync via block_on on the thread-local tokio runtime)
        let connection = crate::async_rt::block_on(async {
            let connecting = endpoint
                .connect(server_addr, &host_str)
                .map_err(|e| format!("quic-session: connect error: {}", e))?;
            connecting
                .await
                .map_err(|e| format!("quic-session: handshake failed: {}", e))
        })
        .map_err(|msg| EvalError::user_error(msg, call_span.clone()))?;

        ok_val(
            Value::QuicSession {
                conn: Arc::new(connection),
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `quic-open-stream`: Open a bidirectional QUIC stream on an existing session.
///
/// Takes `(quic_session)`. Returns a `Handle` with `Readable`, `Writable`, `Binary`,
/// and `Stream` capabilities — the same interface as a TCP Handle.
///
/// Both halves bridge async quinn I/O to synchronous BufRead/Write via block_on.
pub(crate) fn builtin_quic_open_stream(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    // Stubbed: returned Value::Handle which no longer exists.
    // Will be reimplemented with a new stream type in the network redesign sprint.
    let BuiltinArgs { call_span, .. } = ctx_arg;
    Box::pin(async move {
        Err(EvalError::user_error(
            "quic-open-stream: network not yet available — stream redesign in progress".to_string(),
            call_span,
        )
        .into())
    })
}

/// `quic-open-datagram`: Datagram channel on a QUIC session.
///
/// Takes `(quic_session)`. Wraps the connection in a `QuicDatagramHandle`
/// for unreliable QUIC datagram send/recv (RFC 9221).
///
/// The `send-datagram` and `recv-datagram` builtins handle `QuicDatagramHandle`
/// via `conn.send_datagram()` and `conn.read_datagram()` on the underlying
/// Quinn connection. For reliable streaming, use `quic-open-stream` instead.
pub(crate) fn builtin_quic_open_datagram(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;

        reject_named("quic-open-datagram", named.as_ref(), call_span.clone())?;

        if args.len() != 1 {
            return Err(EvalError::user_error(
                format!(
                    "quic-open-datagram: expected 1 argument (quic_session), got {}",
                    args.len()
                ),
                call_span,
            )
            .into());
        }

        let session_val = Arc::clone(&args[0]).require_value()?.clone();
        let conn = match session_val {
            Value::QuicSession { conn: c, .. } => c,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "quic-open-datagram".to_string(),
                    "QuicSession",
                    other.type_name(),
                    call_span.clone(),
                )
                .into())
            }
        };

        ok_val(
            Value::QuicDatagramHandle {
                conn,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `http2-session`: Establish an HTTP/2 session using reqwest.
///
/// Takes `(cap, base_url)` where:
/// - `cap`: NetCap capability controlling which hosts may be contacted
/// - `base_url`: String — `scheme://host[:port]` origin (e.g. `"https://api.example.com"`)
///
/// Returns an `Http2Session` wrapping a `reqwest::Client` (async) configured
/// to prefer HTTP/2 via ALPN for HTTPS connections. The client reuses the
/// underlying connection pool across multiple `http-request` calls.
pub(crate) fn builtin_http2_session(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;

        reject_named("http2-session", named.as_ref(), call_span.clone())?;

        if args.len() != 2 {
            return Err(EvalError::user_error(
                format!(
                    "http2-session: expected 2 arguments (cap base_url), got {}",
                    args.len()
                ),
                call_span,
            )
            .into());
        }

        // All args pre-materialized by force_count
        let cap_val = Arc::clone(&args[0]).require_value()?.clone();
        let url_val = Arc::clone(&args[1]).require_value()?.clone();

        // Validate cap
        let entries = match cap_val {
            Value::NetCap { entries: e, .. } => e,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "http2-session".to_string(),
                    "NetCap",
                    other.type_name(),
                    call_span.clone(),
                )
                .into())
            }
        };

        let base_url = require_string("http2-session", url_val, call_span.clone())?;

        // Parse the base_url to extract host and port for cap validation.
        // We need a host for the allowlist check. Parse scheme://host[:port].
        let (host, port) = parse_origin_host_port(&base_url, call_span.clone())?;

        check_net_cap_allowlist(&entries, &host, port, call_span.clone())?;

        // Build the async reqwest client. Use rustls TLS (already the default via
        // the "rustls" feature flag in Cargo.toml with default-features = false).
        // The client automatically negotiates HTTP/2 via ALPN for HTTPS connections.
        //
        // IMPORTANT: We use reqwest::Client (async) rather than reqwest::blocking::Client
        // to avoid a panic on drop. reqwest::blocking::Client creates an internal tokio
        // runtime. Dropping that runtime from inside an async CEK context panics with
        // "Cannot drop a runtime in a context where blocking is not allowed."
        // The async client piggybacks on the existing outer tokio runtime and is safe to
        // drop from any context.

        // Use reqwest's built-in rustls TLS setup. System CA roots are used by default
        // (rustls-platform-verifier on Linux loads from the system cert store).
        // The ring crypto provider is installed as the process default in main() to
        // resolve the ring/aws-lc-rs ambiguity.
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .user_agent("tinct/0.1 (https://github.com/anthropics/tinct)")
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| {
                EvalError::user_error(
                    format!("http2-session: failed to build HTTP client: {}", e),
                    call_span.clone(),
                )
            })?;

        ok_val(
            Value::Http2Session {
                client: Arc::new(client),
                base_url,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// Parse `scheme://host[:port]` into `(host, Option<port>)`.
///
/// When no explicit port is present, infers the default: 443 for https, 80 for http.
/// This ensures `check_net_cap_allowlist` with `HostPort` entries works correctly
/// for standard URLs that omit the port.
///
/// Returns a hard error if the string cannot be parsed as an origin.
fn parse_origin_host_port(origin: &str, span: Span) -> EvalResult<(String, Option<u16>)> {
    // Strip scheme and record default port
    let (after_scheme, default_port) = if let Some(rest) = origin.strip_prefix("https://") {
        (rest, 443u16)
    } else if let Some(rest) = origin.strip_prefix("http://") {
        (rest, 80u16)
    } else {
        return Err(EvalError::user_error(
            format!(
                "http2-session: base_url must start with http:// or https://, got: {}",
                origin
            ),
            span,
        )
        .into());
    };

    // Strip any trailing path
    let host_part = after_scheme.split('/').next().unwrap_or(after_scheme);

    // Split host:port — use rfind so IPv6 literals (no port) aren't split on ':'.
    if let Some(colon) = host_part.rfind(':') {
        let candidate_port = &host_part[colon + 1..];
        // Only treat it as a port if it's all digits (avoids splitting IPv6 addresses).
        if candidate_port.chars().all(|c| c.is_ascii_digit()) {
            let host = host_part[..colon].to_string();
            let port = candidate_port.parse::<u16>().map_err(|_| {
                EvalError::user_error(
                    format!("http2-session: invalid port in base_url: {}", origin),
                    span,
                )
            })?;
            return Ok((host, Some(port)));
        }
    }
    // No explicit port — use scheme default for allowlist checking.
    Ok((host_part.to_string(), Some(default_port)))
}

/// `http3-session`: Establish an HTTP/3 session over a QUIC connection.
///
/// Takes `(quic_session)`. The QUIC connection's ALPN must include "h3" (set
/// automatically by `quic-session` unless overridden). Performs the HTTP/3
/// handshake and returns an `Http3Session` that can be passed to `http-request`.
///
/// Implementation: wraps quinn::Connection in h3_quinn::Connection, then drives
/// the h3::client handshake via block_on. The returned SendRequest is stored in
/// the Http3Session value.
pub(crate) fn builtin_http3_session(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;

        reject_named("http3-session", named.as_ref(), call_span.clone())?;

        if args.len() != 1 {
            return Err(EvalError::user_error(
                format!(
                    "http3-session: expected 1 argument (quic_session), got {}",
                    args.len()
                ),
                call_span,
            )
            .into());
        }

        let session_val = Arc::clone(&args[0]).require_value()?.clone();

        let conn = match session_val {
            Value::QuicSession { conn: c, .. } => c,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "http3-session".to_string(),
                    "QuicSession",
                    other.type_name(),
                    call_span.clone(),
                )
                .into())
            }
        };

        // Adapt the quinn connection into an h3-quinn connection, then build the H3 client.
        // `h3_quinn::Connection::new` takes ownership of a `quinn::Connection`.
        // We clone the connection — quinn::Connection is Clone and the clone shares
        // the same underlying QUIC connection state.
        let quic_conn = (*conn).clone();
        let h3_conn = h3_quinn::Connection::new(quic_conn);

        // Drive the HTTP/3 handshake: returns (h3::client::Connection driver, SendRequest).
        // The driver must be polled concurrently with request streams to process incoming
        // QUIC frames (SETTINGS, GOAWAY, server push, etc.).
        let (mut driver, send_request) =
            crate::async_rt::block_on(h3::client::builder().build(h3_conn)).map_err(|e| {
                EvalError::user_error(
                    format!("http3-session: HTTP/3 handshake failed: {}", e),
                    call_span.clone(),
                )
            })?;

        // Spawn the driver as a local task so it is polled on every subsequent
        // `async_rt::block_on` call (cooperative multitasking on the current-thread runtime).
        // The JoinHandle is stored in Http3SessionState; dropping it aborts the driver task
        // when the session is dropped (Arc refcount reaches zero).
        //
        // h3 0.0.8: `h3::client::Connection::poll_close(cx)` processes incoming QUIC frames
        // (SETTINGS, GOAWAY, server push, connection error) and returns `Poll::Ready` when
        // the connection closes. The h3 docs say: "It needs to be polled continuously via
        // poll_close()." We wrap it in `std::future::poll_fn` to make it a proper `Future`.
        let driver_handle = crate::async_rt::spawn_local(async move {
            std::future::poll_fn(|cx| {
                // poll_close returns Poll<ConnectionError> — ignore the error value;
                // the request side will surface errors on the next send_request call.
                driver.poll_close(cx).map(|_| ())
            })
            .await
        });

        use crate::value::Http3SessionState;
        ok_val(
            Value::Http3Session {
                session: Arc::new(Mutex::new(Http3SessionState {
                    send_request,
                    _driver: driver_handle,
                })),
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `http-request`: Issue an HTTP request on an HTTP/2 or HTTP/3 session.
/// Takes `(session, method, path, headers, body)`.
///
/// Returns `{status: Int, headers: Dict, body: String}` on success.
/// Raises on network errors so callers can wrap with `[try [fn [] [http-request ...]]]`
/// for Result-based error handling.
///
/// Dispatches on session type:
/// - `Http2Session`: uses reqwest blocking client (HTTP/2 via ALPN)
/// - `Http3Session`: uses h3 over the existing QUIC connection
/// - Other: type error (hard error)
pub(crate) fn builtin_http_request(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        reject_named("http-request", named.as_ref(), call_span.clone())?;

        if args.len() != 5 {
            return Err(EvalError::user_error(
                format!(
                    "http-request: expected 5 arguments (session method path headers body), got {}",
                    args.len()
                ),
                call_span,
            )
            .into());
        }

        // All args pre-materialized by force_count
        let session_val = Arc::clone(&args[0]).require_value()?.clone();
        let method_val = Arc::clone(&args[1]).require_value()?.clone();
        let path_val = Arc::clone(&args[2]).require_value()?.clone();
        let headers_val = Arc::clone(&args[3]).require_value()?.clone();
        let body_val = Arc::clone(&args[4]).require_value()?.clone();

        let method_str = require_string("http-request", method_val, call_span.clone())?;
        let path_str = require_string("http-request", path_val, call_span.clone())?;
        let body_str = require_string("http-request", body_val, call_span.clone())?;

        // Collect request headers from the Dict argument.
        // Each value is an Arc<Thunk> — materialize to extract the string.
        let req_headers: Vec<(String, String)> = match headers_val {
            Value::Dict {
                entries: ref map, ..
            } => {
                let mut out = Vec::with_capacity(map.len());
                for (key, thunk) in map.iter() {
                    let key_str = key.to_string();
                    let val_materialized =
                        crate::eval::materialize(thunk, Some(&call_span), &ctx).await?;
                    let val_str = require_string(
                        "http-request header value",
                        val_materialized,
                        call_span.clone(),
                    )?;
                    out.push((key_str, val_str));
                }
                out
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "http-request".to_string(),
                    "Dict",
                    other.type_name(),
                    call_span.clone(),
                )
                .into())
            }
        };

        match session_val {
            Value::Http3Session {
                session: session_rc,
                ..
            } => http_request_h3(
                session_rc,
                method_str,
                path_str,
                req_headers,
                body_str,
                call_span,
                &ctx,
            ),
            Value::Http2Session {
                client, base_url, ..
            } => {
                http_request_h2(&Http2RequestConfig {
                    client: &client,
                    base_url: &base_url,
                    method_str: &method_str,
                    path_str: &path_str,
                    req_headers: &req_headers,
                    body_str: &body_str,
                    span: call_span,
                })
                .await
            }
            other => Err(EvalError::type_mismatch_ctx(
                "http-request".to_string(),
                "Http2Session or Http3Session",
                other.type_name(),
                call_span.clone(),
            )
            .into()),
        }
    })
}

/// Configuration for HTTP/2 requests.
struct Http2RequestConfig<'a> {
    client: &'a Arc<reqwest::Client>,
    base_url: &'a str,
    method_str: &'a str,
    path_str: &'a str,
    req_headers: &'a [(String, String)],
    body_str: &'a str,
    span: crate::ast::Span,
}

/// Issue an HTTP/2 (or HTTP/1.1) request using a `reqwest::Client` (async).
///
/// The client was configured in `builtin_http2_session` to prefer HTTP/2 via ALPN.
/// Path is resolved relative to `base_url` (the origin stored in the session).
/// Returns `{status: Int, headers: Dict, body: String}` on success or raises on error.
///
/// Uses the async reqwest API and `.await` — safe to call from within the async CEK loop.
/// The async client does not create an internal tokio runtime, so it can be dropped
/// from any async context without panic.
async fn http_request_h2(config: &Http2RequestConfig<'_>) -> EvalResult<Arc<Thunk>> {
    let client = config.client;
    let base_url = config.base_url;
    let method_str = config.method_str;
    let path_str = config.path_str;
    let req_headers = config.req_headers;
    let body_str = config.body_str;
    let span = config.span.clone();
    // Build the full URL: base_url + path_str.
    // If path_str starts with http:// or https://, use it as-is (absolute URL).
    // Otherwise, join with base_url.
    let url = if path_str.starts_with("http://") || path_str.starts_with("https://") {
        path_str.to_string()
    } else {
        let base = base_url.trim_end_matches('/');
        let path = if path_str.starts_with('/') {
            path_str.to_string()
        } else {
            format!("/{}", path_str)
        };
        format!("{}{}", base, path)
    };

    // Build the reqwest request.
    let method = match reqwest::Method::from_bytes(method_str.as_bytes()) {
        Ok(m) => m,
        Err(e) => {
            return Err(EvalError::user_error(
                format!("http-request: invalid HTTP method '{}': {}", method_str, e),
                span,
            )
            .into());
        }
    };

    let mut builder = client.request(method, &url);
    for (k, v) in req_headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    if !body_str.is_empty() {
        builder = builder.body(body_str.to_string());
    }

    let response = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            return Err(EvalError::user_error(
                format!("http-request: request failed: {}", e),
                span,
            )
            .into());
        }
    };

    let status = response.status().as_u16() as i64;

    // Collect response headers.
    let mut headers_map = IndexMap::new();
    for (name, value) in response.headers() {
        let k = crate::value::HashableValue::Str(name.as_str().into());
        let v = match value.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => String::from_utf8_lossy(value.as_bytes()).into_owned(),
        };
        headers_map.insert(k, ok_val(string_val(&v), span.clone())?);
    }

    // Collect body as a String (UTF-8, lossy).
    let body_bytes = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return Err(EvalError::user_error(
                format!("http-request: failed to read response body: {}", e),
                span,
            )
            .into());
        }
    };
    let body_string = String::from_utf8_lossy(&body_bytes).into_owned();

    // Build response dict: {status: Int, headers: Dict, body: String}
    let mut inner = IndexMap::new();
    inner.insert(
        crate::value::HashableValue::Str("status".into()),
        ok_val(
            Value::Int {
                n: status,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        )?,
    );
    inner.insert(
        crate::value::HashableValue::Str("headers".into()),
        ok_val(
            Value::Dict {
                entries: headers_map,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        )?,
    );
    inner.insert(
        crate::value::HashableValue::Str("body".into()),
        ok_val(string_val(&body_string), span.clone())?,
    );

    // Return {status: Int, headers: Dict, body: String} directly.
    // Users who want Result-based error handling wrap the call in [try [fn [] [http-request ...]]].
    ok_val(
        Value::Dict {
            entries: inner,
            type_val: crate::value::unknown_type_val(),
        },
        span,
    )
}

/// Issue an HTTP/3 request on an existing `h3::client::SendRequest` session.
///
/// Builds the `http::Request`, sends it, collects the response headers and body,
/// and returns `{status: Int, headers: Dict, body: String}` on success or raises on error.
fn http_request_h3(
    session_rc: Arc<Mutex<crate::value::Http3SessionState>>,
    method_str: String,
    path_str: String,
    req_headers: Vec<(String, String)>,
    body_str: String,
    span: crate::ast::Span,
    _ctx: &crate::eval::EvalContext,
) -> EvalResult<Arc<Thunk>> {
    use bytes::Bytes;

    // Build the http::Request — body is sent separately as DATA frames.
    let mut builder = http::Request::builder()
        .method(method_str.as_str())
        .uri(path_str.as_str());
    for (k, v) in &req_headers {
        builder = builder.header(k.as_str(), v.as_str());
    }
    let request = match builder.body(()) {
        Ok(r) => r,
        Err(e) => {
            return Err(EvalError::user_error(
                format!("http-request: invalid request: {}", e),
                span,
            )
            .into());
        }
    };

    // Send request headers; get back a RequestStream.
    // lock() accesses send_request inside Http3SessionState.
    // Safe — single-threaded; no other lock during block_on.
    let mut stream = match crate::async_rt::block_on(
        session_rc
            .lock()
            .unwrap()
            .send_request
            .send_request(request),
    ) {
        Ok(s) => s,
        Err(e) => {
            return Err(EvalError::user_error(
                format!("http-request: send_request failed: {}", e),
                span,
            )
            .into());
        }
    };

    // Send the body as a DATA frame (empty body is a zero-length frame).
    if !body_str.is_empty() {
        if let Err(e) =
            crate::async_rt::block_on(stream.send_data(Bytes::from(body_str.into_bytes())))
        {
            return Err(EvalError::user_error(
                format!("http-request: send_data failed: {}", e),
                span,
            )
            .into());
        }
    }

    // Signal end of request stream (no trailers).
    if let Err(e) = crate::async_rt::block_on(stream.finish()) {
        return Err(
            EvalError::user_error(format!("http-request: finish failed: {}", e), span).into(),
        );
    }

    // Receive response headers.
    let response = match crate::async_rt::block_on(stream.recv_response()) {
        Ok(r) => r,
        Err(e) => {
            return Err(EvalError::user_error(
                format!("http-request: recv_response failed: {}", e),
                span,
            )
            .into());
        }
    };

    let status = response.status().as_u16() as i64;

    // Collect response headers into an LLT dict.
    let mut headers_map = IndexMap::new();
    for (name, value) in response.headers() {
        let k = crate::value::HashableValue::Str(name.as_str().into());
        let v = match value.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => {
                // Non-UTF-8 header value — use lossy conversion.
                String::from_utf8_lossy(value.as_bytes()).into_owned()
            }
        };
        headers_map.insert(k, ok_val(string_val(&v), span.clone())?);
    }

    // Collect response body DATA frames.
    // recv_data() returns `impl Buf` — use the Buf trait to copy bytes out.
    let mut body_bytes: Vec<u8> = Vec::new();
    loop {
        match crate::async_rt::block_on(stream.recv_data()) {
            Ok(Some(mut chunk)) => {
                use bytes::Buf;
                while chunk.has_remaining() {
                    let slice = chunk.chunk();
                    body_bytes.extend_from_slice(slice);
                    let n = slice.len();
                    chunk.advance(n);
                }
            }
            Ok(None) => break,
            Err(e) => {
                return Err(EvalError::user_error(
                    format!("http-request: recv_data failed: {}", e),
                    span,
                )
                .into());
            }
        }
    }

    let body_string = String::from_utf8_lossy(&body_bytes).into_owned();

    // Build response dict: {status: Int, headers: Dict, body: String}
    let mut inner = IndexMap::new();
    inner.insert(
        crate::value::HashableValue::Str("status".into()),
        ok_val(
            Value::Int {
                n: status,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        )?,
    );
    inner.insert(
        crate::value::HashableValue::Str("headers".into()),
        ok_val(
            Value::Dict {
                entries: headers_map,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        )?,
    );
    inner.insert(
        crate::value::HashableValue::Str("body".into()),
        ok_val(string_val(&body_string), span.clone())?,
    );

    // Return {status: Int, headers: Dict, body: String} directly.
    // Users who want Result-based error handling wrap the call in [try [fn [] [http-request ...]]].
    ok_val(
        Value::Dict {
            entries: inner,
            type_val: crate::value::unknown_type_val(),
        },
        span,
    )
}

/// `icmp-ping`: Send an ICMP echo request to a host.
/// Takes `(cap, host, timeout_ms)`.
/// Returns `{latency-ms: Int}` on success or raises on failure.
/// Uses unprivileged ICMP ping sockets (`SOCK_DGRAM + IPPROTO_ICMP`, Linux 3.11+).
/// Users who want Result-based error handling wrap the call in `[try [fn [] [icmp-ping ...]]]`.
pub(crate) fn builtin_icmp_ping(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        reject_named("icmp-ping", named.as_ref(), call_span.clone())?;

        if args.len() != 3 {
            return Err(EvalError::user_error(
                format!(
                    "icmp-ping: expected 3 arguments (cap host timeout-ms), got {}",
                    args.len()
                ),
                call_span,
            )
            .into());
        }

        let cap_val = Arc::clone(&args[0]).require_value()?.clone();
        let host_val = Arc::clone(&args[1]).require_value()?.clone();
        let timeout_val = Arc::clone(&args[2]).require_value()?.clone();

        // Extract NetCap entries
        let entries = match cap_val {
            Value::NetCap { entries: e, .. } => e,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "icmp-ping".to_string(),
                    "NetCap",
                    other.type_name(),
                    call_span.clone(),
                )
                .into())
            }
        };

        let host = require_string("icmp-ping", host_val, call_span.clone())?;

        let timeout_ms = match timeout_val {
            Value::Int { n, .. } if n >= 0 => n,
            Value::Int { .. } => {
                return Err(EvalError::user_error(
                    "icmp-ping: timeout-ms must be a non-negative integer".to_string(),
                    call_span.clone(),
                )
                .into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "icmp-ping".to_string(),
                    "Int",
                    other.type_name(),
                    call_span.clone(),
                )
                .into())
            }
        };

        // Validate host against NetCap allowlist (ICMP has no port, pass None)
        // This fires before any socket operations.
        check_net_cap_allowlist(&entries, &host, None, call_span.clone())?;

        // Perform platform-specific ping and return result dict
        icmp_ping_impl(&host, timeout_ms, call_span, &ctx)
    })
}

#[cfg(unix)]
fn icmp_ping_impl(
    host: &str,
    timeout_ms: i64,
    span: Span,
    _ctx: &crate::eval::EvalContext,
) -> EvalResult<Arc<Thunk>> {
    use std::net::ToSocketAddrs;

    // Resolve hostname to IPv4 address
    let addr = match (host, 0u16).to_socket_addrs() {
        Ok(mut iter) => {
            // Find the first IPv4 address
            match iter.find(|a| a.is_ipv4()) {
                Some(a) => a,
                None => {
                    return Err(EvalError::user_error(
                        format!("icmp-ping: no IPv4 address found for '{}'", host),
                        span,
                    )
                    .into());
                }
            }
        }
        Err(e) => {
            return Err(EvalError::user_error(
                format!("icmp-ping: failed to resolve '{}': {}", host, e),
                span,
            )
            .into());
        }
    };

    // Create unprivileged ICMP socket (SOCK_DGRAM + IPPROTO_ICMP, Linux 3.11+)
    let sock_fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP) };
    if sock_fd < 0 {
        let os_err = std::io::Error::last_os_error();
        return Err(EvalError::user_error(
            format!(
                "icmp-ping: failed to create ICMP socket ({}): \
                 kernel may require net.ipv4.ping_group_range to include your GID",
                os_err
            ),
            span,
        )
        .into());
    }

    // RAII guard to close the socket on any exit path
    struct SockGuard(libc::c_int);
    impl Drop for SockGuard {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.0);
            }
        }
    }
    let _guard = SockGuard(sock_fd);

    // Set receive timeout via SO_RCVTIMEO
    let timeout_secs = timeout_ms / 1000;
    let timeout_usecs = (timeout_ms % 1000) * 1000;

    // Bounds check for platforms where time_t is 32-bit (prevents silent truncation)
    if !(libc::time_t::MIN..=libc::time_t::MAX).contains(&timeout_secs) {
        return Err(EvalError::user_error(
            format!(
                "icmp-ping: timeout-ms too large for platform (max {} seconds)",
                libc::time_t::MAX
            ),
            span,
        )
        .into());
    }

    let tv = libc::timeval {
        tv_sec: timeout_secs as libc::time_t,
        tv_usec: timeout_usecs as libc::suseconds_t,
    };
    let ret = unsafe {
        libc::setsockopt(
            sock_fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const libc::timeval as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        let os_err = std::io::Error::last_os_error();
        return Err(EvalError::user_error(
            format!("icmp-ping: setsockopt SO_RCVTIMEO failed ({})", os_err),
            span,
        )
        .into());
    }

    // Build ICMP Echo Request packet
    // Format: type(1) code(1) checksum(2) id(2) seq(2) data(...)
    let id = (std::process::id() & 0xFFFF) as u16;
    let seq: u16 = 1;
    const DATA: &[u8] = b"tinct-ping";
    let mut packet = vec![0u8; 8 + DATA.len()];
    packet[0] = 8; // ICMP Echo Request type
    packet[1] = 0; // code
    packet[2] = 0; // checksum (computed below)
    packet[3] = 0;
    packet[4] = (id >> 8) as u8;
    packet[5] = (id & 0xFF) as u8;
    packet[6] = (seq >> 8) as u8;
    packet[7] = (seq & 0xFF) as u8;
    packet[8..].copy_from_slice(DATA);

    // Compute ICMP checksum (RFC 792)
    let checksum = icmp_checksum(&packet);
    packet[2] = (checksum >> 8) as u8;
    packet[3] = (checksum & 0xFF) as u8;

    // Build destination sockaddr_in
    let ip_octets = match addr.ip() {
        std::net::IpAddr::V4(v4) => v4.octets(),
        std::net::IpAddr::V6(_) => {
            return Err(EvalError::user_error(
                "icmp-ping: IPv6 is not yet supported".to_string(),
                span,
            )
            .into());
        }
    };
    let dest = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            // s_addr holds the IPv4 address as raw bytes in network order.
            // from_ne_bytes reinterprets the octets as a native-endian u32, so
            // the bytes land in memory in the original [a,b,c,d] order on any
            // architecture — correct for s_addr regardless of host endianness.
            s_addr: u32::from_ne_bytes(ip_octets),
        },
        sin_zero: [0; 8],
    };

    // Record start time
    let start = std::time::Instant::now();

    // Send ICMP Echo Request
    if packet.len() > u32::MAX as usize {
        return Err(EvalError::user_error(
            "icmp-ping: packet too large for platform".to_string(),
            span.clone(),
        )
        .into());
    }
    let sent = unsafe {
        libc::sendto(
            sock_fd,
            packet.as_ptr() as *const libc::c_void,
            packet.len(),
            0,
            &dest as *const libc::sockaddr_in as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if sent < 0 {
        let os_err = std::io::Error::last_os_error();
        return Err(
            EvalError::user_error(format!("icmp-ping: sendto failed ({})", os_err), span).into(),
        );
    }

    // Receive ICMP Echo Reply
    // With SOCK_DGRAM + IPPROTO_ICMP, kernel strips the IP header — reply is ICMP only
    let mut recv_buf = [0u8; 256];
    let recvd = unsafe {
        libc::recv(
            sock_fd,
            recv_buf.as_mut_ptr() as *mut libc::c_void,
            recv_buf.len(),
            0,
        )
    };

    let elapsed = start.elapsed();
    let latency_ms = elapsed.as_millis() as i64;

    if recvd < 0 {
        let os_err = std::io::Error::last_os_error();
        // EAGAIN / EWOULDBLOCK = timeout
        let raw_errno = os_err.raw_os_error().unwrap_or(0);
        if raw_errno == libc::EAGAIN || raw_errno == libc::EWOULDBLOCK {
            return Err(EvalError::user_error(
                format!("icmp-ping: timeout after {}ms", timeout_ms),
                span,
            )
            .into());
        }
        return Err(
            EvalError::user_error(format!("icmp-ping: recv failed ({})", os_err), span).into(),
        );
    }

    // Validate reply: must be at least 8 bytes, type=0 (Echo Reply)
    let recvd = recvd as usize;
    if recvd < 8 {
        return Err(EvalError::user_error(
            "icmp-ping: received truncated ICMP reply".to_string(),
            span,
        )
        .into());
    }
    if recv_buf[0] != 0 {
        // Not an Echo Reply (type 0); could be a Destination Unreachable etc.
        return Err(EvalError::user_error(
            format!("icmp-ping: unexpected ICMP reply type {}", recv_buf[0]),
            span,
        )
        .into());
    }

    // Return {latency-ms: Int} directly.
    // Users who want Result-based error handling wrap the call in [try [fn [] [icmp-ping ...]]].
    use crate::value::HashableValue;
    let mut result = IndexMap::new();
    result.insert(
        HashableValue::Str("latency-ms".into()),
        ok_val(
            Value::Int {
                n: latency_ms,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        )?,
    );
    ok_val(
        Value::Dict {
            entries: result,
            type_val: crate::value::unknown_type_val(),
        },
        span,
    )
}

/// Compute ICMP checksum per RFC 792: one's complement sum of 16-bit words.
#[cfg(unix)]
fn icmp_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        let word = ((data[i] as u32) << 8) | (data[i + 1] as u32);
        sum = sum.wrapping_add(word);
        i += 2;
    }
    // Handle odd byte
    if i < data.len() {
        sum = sum.wrapping_add((data[i] as u32) << 8);
    }
    // Fold carries
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(not(unix))]
fn icmp_ping_impl(
    _host: &str,
    _timeout_ms: i64,
    span: Span,
    _ctx: &crate::eval::EvalContext,
) -> EvalResult<Arc<Thunk>> {
    Err(EvalError::user_error(
        "icmp-ping: ICMP ping is not supported on this platform".to_string(),
        span,
    )
    .into())
}

/// `send-datagram`: Send a message over a DatagramHandle.
/// Signature: `[send-datagram data handle]` → null
/// `data` must be a String or Bytes.
pub(crate) fn builtin_send_datagram(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;

        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("send-datagram", named.as_ref(), call_span.clone())?;

        let data_val = Arc::clone(&args[0]).require_value()?.clone();
        let handle_val = Arc::clone(&args[1]).require_value()?.clone();

        // Extract bytes to send (String or Bytes) — common to all handle variants.
        let data_bytes: Vec<u8> = match data_val {
            Value::String {
                source, start, end, ..
            } => source[start..end].as_bytes().to_vec(),
            Value::Bytes {
                source, start, end, ..
            } => source[start..end].to_vec(),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "send-datagram".to_string(),
                    "String or Bytes",
                    other.type_name(),
                    call_span.clone(),
                )
                .into())
            }
        };

        match handle_val {
            // QUIC unreliable datagram (RFC 9221) — async send via block_on.
            // `conn.send_datagram` returns immediately; the underlying QUIC stack
            // handles retransmission of the UDP packet if necessary (implementation-defined).
            // Returns `SendDatagramError::UnsupportedByPeer` if the remote did not advertise
            // datagram support in its transport parameters.
            Value::QuicDatagramHandle { conn, .. } => {
                let payload = bytes::Bytes::from(data_bytes);
                crate::async_rt::block_on(conn.send_datagram_wait(payload)).map_err(|e| {
                    EvalError::user_error(
                        format!("send-datagram: QUIC datagram send failed: {}", e),
                        call_span.clone(),
                    )
                })?;
                ok_val(
                    Value::Dict {
                        entries: IndexMap::new(),
                        type_val: crate::value::unknown_type_val(),
                    },
                    call_span,
                )
            }

            other => Err(EvalError::type_mismatch_ctx(
                "send-datagram".to_string(),
                "QuicDatagramHandle",
                other.type_name(),
                call_span.clone(),
            )
            .into()),
        }
    })
}

/// `recv-datagram`: Receive a message from a DatagramHandle.
/// Signature: `[recv-datagram handle]` → `{data: String}`
/// The socket must have been put into non-blocking mode or have a timeout set
/// via the underlying OS to avoid blocking forever; this builtin blocks until
/// a datagram arrives.
pub(crate) fn builtin_recv_datagram(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        let val = crate::builtins::expect_one_arg(
            "recv-datagram",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

        use crate::value::HashableValue;

        // Helper: build the `{data: Bytes}` result dict from a received byte buffer.
        let make_data_dict = |buf: Vec<u8>| -> EvalResult<Arc<Thunk>> {
            let data_len = buf.len();
            let data_bytes = Value::Bytes {
                source: Arc::from(buf.as_slice()),
                start: 0,
                end: data_len,
                type_val: crate::value::unknown_type_val(),
            };
            let mut dict = IndexMap::new();
            dict.insert(
                HashableValue::Str("data".into()),
                ok_val(data_bytes, call_span.clone())?,
            );
            ok_val(
                Value::Dict {
                    entries: dict,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )
        };

        match val {
            // QUIC unreliable datagram (RFC 9221) — async recv via block_on.
            // `conn.read_datagram()` returns the next datagram payload as a `bytes::Bytes`.
            // Blocks until a datagram arrives or the connection closes.
            Value::QuicDatagramHandle { conn, .. } => {
                let payload = crate::async_rt::block_on(conn.read_datagram()).map_err(|e| {
                    EvalError::user_error(
                        format!("recv-datagram: QUIC datagram recv failed: {}", e),
                        call_span.clone(),
                    )
                })?;
                make_data_dict(payload.to_vec())
            }

            other => Err(EvalError::type_mismatch_ctx(
                "recv-datagram".to_string(),
                "QuicDatagramHandle",
                other.type_name(),
                call_span.clone(),
            )
            .into()),
        }
    })
}

// ── URI parsing builtins ───────────────────────────────────────────────────────

/// Parse any URI string → Uri dict
///
/// Returns a Dict with: scheme, username, password, host, port, path, query, fragment.
/// host/port are null for non-hierarchical URIs (mailto:, tel:, urn:, news:).
/// username/password extracted by splitting userinfo on ":" (RFC 3986 convention).
pub(crate) fn builtin_uri(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        let val = expect_one_arg("uri", &args, named.as_ref(), &ctx, call_span.clone())?;
        let s = match val {
            Value::String {
                ref source,
                start,
                end,
                ..
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
                HashableValue::Str("scheme".into()),
                ok_val(string_val(parsed.scheme()), call_span.clone())?,
            );

            // username (split from userinfo)
            let username = if parsed.username().is_empty() {
                Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }
            } else {
                string_val(parsed.username())
            };
            dict.insert(
                HashableValue::Str("username".into()),
                ok_val(username, call_span.clone())?,
            );

            // password (split from userinfo)
            let password = match parsed.password() {
                Some(pw) => string_val(pw),
                None => Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                },
            };
            dict.insert(
                HashableValue::Str("password".into()),
                ok_val(password, call_span.clone())?,
            );

            // host (null for non-hierarchical; strip IPv6 brackets)
            let host = match parsed.host_str() {
                Some(h) => string_val(h),
                None => Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                },
            };
            dict.insert(
                HashableValue::Str("host".into()),
                ok_val(host, call_span.clone())?,
            );

            // port (null if not specified)
            let port = match parsed.port() {
                Some(p) => Value::Int {
                    n: i64::from(p),
                    type_val: crate::value::unknown_type_val(),
                },
                None => Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                },
            };
            dict.insert(
                HashableValue::Str("port".into()),
                ok_val(port, call_span.clone())?,
            );

            // path (always present per RFC 3986)
            dict.insert(
                HashableValue::Str("path".into()),
                ok_val(string_val(parsed.path()), call_span.clone())?,
            );

            // query (null if absent)
            let query = match parsed.query() {
                Some(q) => string_val(q),
                None => Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                },
            };
            dict.insert(
                HashableValue::Str("query".into()),
                ok_val(query, call_span.clone())?,
            );

            // fragment (null if absent)
            let fragment = match parsed.fragment() {
                Some(f) => string_val(f),
                None => Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                },
            };
            dict.insert(
                HashableValue::Str("fragment".into()),
                ok_val(fragment, call_span.clone())?,
            );

            return ok_val(
                Value::Dict {
                    entries: dict,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span,
            );
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
            HashableValue::Str("scheme".into()),
            ok_val(string_val(&scheme.to_lowercase()), call_span.clone())?,
        );

        // Non-hierarchical URIs: all null for userinfo/host/port
        for key in ["username", "password", "host", "port"] {
            dict.insert(
                HashableValue::Str(key.into()),
                ok_val(
                    Value::Dict {
                        entries: IndexMap::new(),
                        type_val: crate::value::unknown_type_val(),
                    },
                    call_span.clone(),
                )?,
            );
        }

        // path is the remaining part after scheme:
        // For mailto:user@example.com, path is "user@example.com"
        // For urn:isbn:123, path is "isbn:123"
        dict.insert(
            HashableValue::Str("path".into()),
            ok_val(string_val(rest), call_span.clone())?,
        );

        // query and fragment: null (non-hierarchical URIs typically don't have these)
        for key in ["query", "fragment"] {
            dict.insert(
                HashableValue::Str(key.into()),
                ok_val(
                    Value::Dict {
                        entries: IndexMap::new(),
                        type_val: crate::value::unknown_type_val(),
                    },
                    call_span.clone(),
                )?,
            );
        }

        ok_val(
            Value::Dict {
                entries: dict,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// Parse hierarchical URL → Url dict
///
/// Errors if no authority (no host). Port defaults to scheme default if not specified.
pub(crate) fn builtin_url(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        let val = expect_one_arg("url", &args, named.as_ref(), &ctx, call_span.clone())?;
        let s = match val {
            Value::String {
                ref source,
                start,
                end,
                ..
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
            HashableValue::Str("scheme".into()),
            ok_val(string_val(parsed.scheme()), call_span.clone())?,
        );

        // username (split from userinfo)
        let username = if parsed.username().is_empty() {
            Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            }
        } else {
            string_val(parsed.username())
        };
        dict.insert(
            HashableValue::Str("username".into()),
            ok_val(username, call_span.clone())?,
        );

        // password (split from userinfo)
        let password = match parsed.password() {
            Some(pw) => string_val(pw),
            None => Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            },
        };
        dict.insert(
            HashableValue::Str("password".into()),
            ok_val(password, call_span.clone())?,
        );

        // host (always present for URLs; unwrap is safe)
        dict.insert(
            HashableValue::Str("host".into()),
            ok_val(string_val(parsed.host_str().unwrap()), call_span.clone())?,
        );

        // port (default to scheme default if not specified)
        let port = parsed.port_or_known_default().unwrap_or({
            // Fallback for unknown schemes: return port 0 as sentinel
            // (url::Url::port_or_known_default returns None for unknown schemes)
            0
        });
        dict.insert(
            HashableValue::Str("port".into()),
            ok_val(
                Value::Int {
                    n: i64::from(port),
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            )?,
        );

        // path (always present per RFC 3986; default to "/" if empty)
        let path = if parsed.path().is_empty() {
            "/"
        } else {
            parsed.path()
        };
        dict.insert(
            HashableValue::Str("path".into()),
            ok_val(string_val(path), call_span.clone())?,
        );

        // query (null if absent)
        let query = match parsed.query() {
            Some(q) => string_val(q),
            None => Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            },
        };
        dict.insert(
            HashableValue::Str("query".into()),
            ok_val(query, call_span.clone())?,
        );

        // fragment (null if absent)
        let fragment = match parsed.fragment() {
            Some(f) => string_val(f),
            None => Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            },
        };
        dict.insert(
            HashableValue::Str("fragment".into()),
            ok_val(fragment, call_span.clone())?,
        );

        ok_val(
            Value::Dict {
                entries: dict,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// Parse URN → Urn dict per RFC 8141
///
/// Returns: nid, nss, r-component, q-component, fragment.
/// Errors if scheme is not "urn".
pub(crate) fn builtin_urn(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        let val = expect_one_arg("urn", &args, named.as_ref(), &ctx, call_span.clone())?;
        let s = match val {
            Value::String {
                ref source,
                start,
                end,
                ..
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
            HashableValue::Str("nid".into()),
            ok_val(string_val(nid), call_span.clone())?,
        );
        dict.insert(
            HashableValue::Str("nss".into()),
            ok_val(string_val(nss), call_span.clone())?,
        );

        // r-component (null if absent)
        let r_val = match r_component {
            Some(r) => string_val(r),
            None => Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            },
        };
        dict.insert(
            HashableValue::Str("r-component".into()),
            ok_val(r_val, call_span.clone())?,
        );

        // q-component (null if absent)
        let q_val = match q_component {
            Some(q) => string_val(q),
            None => Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            },
        };
        dict.insert(
            HashableValue::Str("q-component".into()),
            ok_val(q_val, call_span.clone())?,
        );

        // fragment (null if absent)
        let frag_val = match fragment {
            Some(f) => string_val(f),
            None => Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            },
        };
        dict.insert(
            HashableValue::Str("fragment".into()),
            ok_val(frag_val, call_span.clone())?,
        );

        ok_val(
            Value::Dict {
                entries: dict,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
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
            [Strictness::Seq, Strictness::Seq],
            2
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
