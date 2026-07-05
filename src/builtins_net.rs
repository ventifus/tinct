// Network builtins are stubbed pending the Handle → protocol redesign sprint.
// Internal helpers are dead code until that sprint lands.
#![allow(dead_code)]

//! Net builtin module — network I/O and URI parsing.
//!
//! This module provides:
//! - Network builtin implementations: TCP/UDP/UNIX connections, TLS, QUIC, HTTP, ICMP
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
//! Extracted from `builtins_io.rs` in T-915.

use std::cell::RefCell;

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::{builtin, expect_one_arg, ok_val, reject_named, require_string};

use crate::error::{EvalError, EvalResult};
use crate::type_def::TyConDef;
use crate::types::{Row, Type, TypeEnv};
use crate::value::{string_val, BuiltinArgs, BuiltinDef, HashableValue, Strictness, Thunk, Value};

/// `connect`: Open a TCP or UDP connection within a NetCap.
/// Takes a NetCap, hostname String, port Int, and optional Transport variant (default: Tcp).
/// - `Tcp` (default) → Handle[Binary Readable Writable Stream]
/// - `Udp` → error "UDP not yet supported, use Tcp" (reserved for Phase 2)
pub(crate) fn builtin_connect(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    // Network redesign in progress: Handle/WriteHandle removed. TCP/Unix connections
    // will be reimplemented on a new stream type. See File redesign sprint.
    let BuiltinArgs { call_span, .. } = ctx_arg;
    Box::pin(async move {
        Err(EvalError::user_error(
            "connect: network not yet available — tcp redesign in progress".to_string(),
            call_span,
        )
        .into())
    })
}

// builtin_connect old body removed. Network redesign sprint will rewrite with a new stream type.

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

    // Try to parse host as IP address
    let host_ip = host.parse::<IpAddr>().ok();

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

/// TLS stream wrapper for reading (implements BufRead by delegating to shared TLS stream)
struct TlsReader {
    stream: Rc<RefCell<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>>>,
    buf: Vec<u8>,
    buf_pos: usize,
}

impl std::io::Read for TlsReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut *self.stream.borrow_mut(), buf)
    }
}

impl std::io::BufRead for TlsReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        // If buffer is exhausted, refill it
        if self.buf_pos >= self.buf.len() {
            self.buf.resize(8192, 0);
            let n = std::io::Read::read(&mut *self.stream.borrow_mut(), &mut self.buf[..])?;
            self.buf.truncate(n);
            self.buf_pos = 0;
        }
        Ok(&self.buf[self.buf_pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.buf_pos = std::cmp::min(self.buf_pos + amt, self.buf.len());
        // If buffer fully consumed, clear it
        if self.buf_pos >= self.buf.len() {
            self.buf.clear();
            self.buf_pos = 0;
        }
    }
}

/// TLS stream wrapper for writing (implements Write by delegating to shared TLS stream)
struct TlsWriter {
    stream: Rc<RefCell<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>>>,
}

impl std::io::Write for TlsWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::Write::write(&mut *self.stream.borrow_mut(), buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut *self.stream.borrow_mut())
    }
}

/// Build a rustls ClientConfig from the opts dict
pub(crate) async fn build_tls_config(
    opts_val: &Value,
    opts_span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<rustls::ClientConfig> {
    use rustls::RootCertStore;

    // Install the ring crypto provider if not already installed.
    // rustls 0.23 requires an explicit provider; ring is the default for tinct.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let opts_dict = match opts_val {
        Value::Dict(d) => d,
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
    let no_system_roots = if let Some(thunk_id) =
        opts_dict.get(&crate::value::HashableValue::Str("no-system-roots".into()))
    {
        let thunk = ctx.get_thunk(*thunk_id);
        let val = crate::eval::materialize(&thunk, Some(&opts_span), ctx).await?;
        val.is_truthy()
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
    let mozilla_roots = if let Some(thunk_id) =
        opts_dict.get(&crate::value::HashableValue::Str("mozilla-roots".into()))
    {
        let thunk = ctx.get_thunk(*thunk_id);
        let val = crate::eval::materialize(&thunk, Some(&opts_span), ctx).await?;
        val.is_truthy()
    } else {
        false
    };

    if mozilla_roots {
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }

    // Load ca-bundle if provided
    if let Some(thunk_id) = opts_dict.get(&crate::value::HashableValue::Str("ca-bundle".into())) {
        let thunk = ctx.get_thunk(*thunk_id);
        let handle_val = crate::eval::materialize(&thunk, Some(&opts_span), ctx).await?;
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

        let cert_thunk_id = opts_dict
            .get(&crate::value::HashableValue::Str("client-cert".into()))
            .unwrap();
        let cert_thunk = ctx.get_thunk(*cert_thunk_id);
        let cert_handle = crate::eval::materialize(&cert_thunk, Some(&opts_span), ctx).await?;

        let key_thunk_id = opts_dict
            .get(&crate::value::HashableValue::Str("client-key".into()))
            .unwrap();
        let key_thunk = ctx.get_thunk(*key_thunk_id);
        let key_handle = crate::eval::materialize(&key_thunk, Some(&opts_span), ctx).await?;

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
    if let Some(thunk_id) = opts_dict.get(&crate::value::HashableValue::Str("alpn".into())) {
        let thunk = ctx.get_thunk(*thunk_id);
        let alpn_val = crate::eval::materialize(&thunk, Some(&opts_span), ctx).await?;
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
        Value::Dict(d) => d,
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
    for (_idx, val_id) in map {
        let thunk = ctx.get_thunk(*val_id);
        let v = crate::eval::materialize(&thunk, Some(&span), ctx).await?;
        let protocol_str = match v {
            Value::String { source, start, end } => source[start..end].to_string(),
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

/// Validate SPKI pins against the peer certificate
pub(crate) async fn validate_spki_pins(
    conn: &rustls::ClientConnection,
    pins_val: &Value,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<()> {
    // Extract list of pins from an integer-keyed Dict
    let pins_map = match pins_val {
        Value::Dict(d) => d,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "tls-connect opts.pins".to_string(),
                "Dict of SpkiPin",
                other.type_name(),
                span.clone(),
            )
            .into())
        }
    };
    let mut pins = Vec::new();
    for (_idx, val_id) in pins_map {
        let thunk = ctx.get_thunk(*val_id);
        let pin_val = crate::eval::materialize(&thunk, Some(&span), ctx).await?;
        pins.push(pin_val);
    }

    if pins.is_empty() {
        return Ok(()); // No pins to validate
    }

    // Get leaf certificate
    let peer_certs = conn.peer_certificates().ok_or_else(|| {
        EvalError::user_error(
            "tls-connect: no peer certificates available for SPKI pin validation".to_string(),
            span.clone(),
        )
    })?;

    if peer_certs.is_empty() {
        return Err(EvalError::user_error(
            "tls-connect: peer certificate list is empty".to_string(),
            span.clone(),
        )
        .into());
    }

    let leaf_cert = &peer_certs[0];

    // Extract SPKI from certificate and compute hashes (RFC 7469 compliant)

    // Validate at least one pin matches
    let mut matched = false;
    for pin_val in &pins {
        let pin_dict = match pin_val {
            Value::Dict(d) => d,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "tls-connect opts.pins element".to_string(),
                    "SpkiPin dict",
                    other.type_name(),
                    span.clone(),
                )
                .into())
            }
        };

        let algorithm_thunk_id = pin_dict
            .get(&crate::value::HashableValue::Str("algorithm".into()))
            .ok_or_else(|| {
                EvalError::user_error(
                    "tls-connect: SpkiPin missing 'algorithm' field".to_string(),
                    span.clone(),
                )
            })?;
        let algorithm_thunk = ctx.get_thunk(*algorithm_thunk_id);
        let algorithm_val = crate::eval::materialize(&algorithm_thunk, Some(&span), ctx).await?;

        let fingerprint_thunk_id = pin_dict
            .get(&crate::value::HashableValue::Str("fingerprint".into()))
            .ok_or_else(|| {
                EvalError::user_error(
                    "tls-connect: SpkiPin missing 'fingerprint' field".to_string(),
                    span.clone(),
                )
            })?;
        let fingerprint_thunk = ctx.get_thunk(*fingerprint_thunk_id);
        let fingerprint_val =
            crate::eval::materialize(&fingerprint_thunk, Some(&span), ctx).await?;

        let algorithm_tag = match algorithm_val {
            Value::Variant { tag, .. } => tag,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "tls-connect opts.pins.algorithm".to_string(),
                    "HashAlgorithm variant",
                    other.type_name(),
                    span.clone(),
                )
                .into())
            }
        };

        let expected_fingerprint = match fingerprint_val {
            Value::Bytes { source, start, end } => source[start..end].to_vec(),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "tls-connect opts.pins.fingerprint".to_string(),
                    "Bytes",
                    other.type_name(),
                    span.clone(),
                )
                .into())
            }
        };

        // Compute hash of certificate using the specified algorithm
        let computed_hash = compute_spki_hash(leaf_cert.as_ref(), &algorithm_tag, span.clone())?;

        if computed_hash == expected_fingerprint {
            matched = true;
            break;
        }
    }

    if !matched {
        return Err(EvalError::user_error(
            "tls-connect: peer certificate SPKI does not match any provided pin".to_string(),
            span,
        )
        .into());
    }

    Ok(())
}

/// Compute SPKI hash (RFC 7469 compliant: hash the SubjectPublicKeyInfo field)
fn compute_spki_hash(cert_der: &[u8], algorithm: &str, span: Span) -> EvalResult<Vec<u8>> {
    use sha3::{Digest, Sha3_256, Sha3_384, Sha3_512};

    // Parse the X.509 certificate and extract the SPKI field
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der).map_err(|e| {
        EvalError::user_error(
            format!("tls-connect: failed to parse certificate: {}", e),
            span.clone(),
        )
    })?;

    // Extract the raw SPKI bytes
    let spki_der = cert.tbs_certificate.subject_pki.raw;

    match algorithm {
        "Sha256" => {
            use sha2::Sha256;
            Ok(Sha256::digest(spki_der).to_vec())
        }
        "Sha384" => {
            use sha2::Sha384;
            Ok(Sha384::digest(spki_der).to_vec())
        }
        "Sha512" => {
            use sha2::Sha512;
            Ok(Sha512::digest(spki_der).to_vec())
        }
        "Sha3-256" => Ok(Sha3_256::digest(spki_der).to_vec()),
        "Sha3-384" => Ok(Sha3_384::digest(spki_der).to_vec()),
        "Sha3-512" => Ok(Sha3_512::digest(spki_der).to_vec()),
        "Blake3" => Ok(blake3::hash(spki_der).as_bytes().to_vec()),
        other => Err(EvalError::user_error(
            format!("tls-connect: unsupported hash algorithm '{}'", other),
            span,
        )
        .into()),
    }
}

/// Extract certificate info for tls-peer-cert
fn extract_cert_info(
    cert_der: &rustls::pki_types::CertificateDer,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Value> {
    // For now, return a minimal dict with just the cert bytes
    // Full X.509 parsing would require a crate like x509-parser or rustls-webpki
    let mut info = IndexMap::new();

    // Store the raw cert DER bytes so tls-peer-cert can parse it later
    use crate::value::HashableValue;
    info.insert(
        HashableValue::Str("_raw_der".into()),
        ctx.alloc_thunk(ok_val(
            Value::Bytes {
                source: Rc::from(cert_der.as_ref()),
                start: 0,
                end: cert_der.len(),
            },
            span,
        )?),
    );

    Ok(Value::Dict(info))
}

/// Extract Common Name (CN) from an X.509 distinguished name
fn extract_cn(name: &x509_parser::x509::X509Name) -> Option<String> {
    use x509_parser::der_parser::oid;
    // OID for commonName is 2.5.4.3
    let cn_oid = oid!(2.5.4 .3);

    for rdn in name.iter() {
        for attr in rdn.iter() {
            if attr.attr_type() == &cn_oid {
                if let Ok(cn_str) = attr.attr_value().as_str() {
                    return Some(cn_str.to_string());
                }
            }
        }
    }
    None
}

/// Extract Subject Alternative Names (SANs) from an X.509 certificate
/// Returns a Seq of strings (DNS names, IPs, emails, URIs)
async fn extract_sans(
    cert: &x509_parser::certificate::X509Certificate<'_>,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Value> {
    use x509_parser::extensions::GeneralName;

    let mut sans_list = Vec::new();

    // Find the SubjectAlternativeName extension
    if let Some(san_ext) = cert
        .tbs_certificate
        .extensions()
        .iter()
        .find(|e| e.oid == x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME)
    {
        // parsed_extension() returns &ParsedExtension, not Result
        if let x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) =
            san_ext.parsed_extension()
        {
            for name in &san.general_names {
                match name {
                    GeneralName::DNSName(dns) => {
                        sans_list.push(string_val(dns));
                    }
                    GeneralName::IPAddress(ip_bytes) => {
                        // Convert IP bytes to string representation
                        let ip_str = if ip_bytes.len() == 4 {
                            format!(
                                "{}.{}.{}.{}",
                                ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3]
                            )
                        } else if ip_bytes.len() == 16 {
                            // IPv6
                            format!(
                                "{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}",
                                ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3],
                                ip_bytes[4], ip_bytes[5], ip_bytes[6], ip_bytes[7],
                                ip_bytes[8], ip_bytes[9], ip_bytes[10], ip_bytes[11],
                                ip_bytes[12], ip_bytes[13], ip_bytes[14], ip_bytes[15]
                            )
                        } else {
                            continue; // Skip malformed IP addresses
                        };
                        sans_list.push(string_val(&ip_str));
                    }
                    GeneralName::RFC822Name(email) => {
                        sans_list.push(string_val(email));
                    }
                    GeneralName::URI(uri) => {
                        sans_list.push(string_val(uri));
                    }
                    _ => {
                        // Ignore other types (DirectoryName, EDIPartyName, etc.)
                    }
                }
            }
        }
    }

    // Build an integer-keyed Dict from the collected SAN values
    let mut dict: indexmap::IndexMap<crate::value::HashableValue, crate::value::ThunkId> =
        indexmap::IndexMap::new();
    for (i, val) in sans_list.into_iter().enumerate() {
        let id = ctx.alloc_thunk(ok_val(val, span.clone())?);
        dict.insert(crate::value::HashableValue::Int(i as i64), id);
    }
    Ok(Value::Dict(dict))
}

/// `tls-layer`: Layer TLS on an existing TCP Handle (STARTTLS use case).
/// Stubbed: Handle/WriteHandle removed. Will be reimplemented with new stream type.
pub(crate) fn builtin_tls_layer(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
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

/// Sync wrapper around a `quinn::RecvStream` that bridges async reads to the
/// synchronous `BufRead` trait expected by `Value::Handle`.
///
/// Each `read` call issues `block_on(recv.read_buf(...))` on the thread-local
/// tokio runtime. This keeps all async I/O on one thread and avoids spawning.
///
/// IP resolution note: the connection uses the IP resolved during `builtin_quic_session`
/// (via `check_net_cap_allowlist` → `server_addr`). The `RecvStream` here is part of an
/// already-established QUIC connection — no re-resolution occurs at read time. DNS-rebinding
/// is therefore not a concern for stream reads.
struct QuicRecvReader {
    recv: quinn::RecvStream,
    buf: Vec<u8>,
    buf_pos: usize,
    /// Running total of bytes received across all reads. Used to enforce the per-stream
    /// byte limit (QUIC_STREAM_BYTE_LIMIT) and prevent unbounded memory accumulation.
    bytes_read: usize,
}

/// Maximum bytes that may be read from a single QUIC stream (64 MiB).
const QUIC_STREAM_BYTE_LIMIT: usize = 64 * 1024 * 1024;

impl std::io::Read for QuicRecvReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.buf_pos < self.buf.len() {
            // Serve from internal buffer first
            let available = self.buf.len() - self.buf_pos;
            let n = available.min(out.len());
            out[..n].copy_from_slice(&self.buf[self.buf_pos..self.buf_pos + n]);
            self.buf_pos += n;
            return Ok(n);
        }
        // Buffer exhausted — fetch more from the stream
        self.buf.clear();
        self.buf_pos = 0;
        self.buf.resize(8192, 0u8);
        let n = crate::async_rt::block_on(self.recv.read(&mut self.buf))
            .map_err(|e| std::io::Error::other(format!("quic recv: {e}")))?
            .unwrap_or(0);
        self.buf.truncate(n);
        self.bytes_read += n;
        if self.bytes_read > QUIC_STREAM_BYTE_LIMIT {
            return Err(std::io::Error::other(format!(
                "quic recv: stream exceeded byte limit ({} bytes > {} MiB limit)",
                self.bytes_read,
                QUIC_STREAM_BYTE_LIMIT / (1024 * 1024),
            )));
        }
        if n == 0 {
            return Ok(0); // EOF
        }
        let take = n.min(out.len());
        out[..take].copy_from_slice(&self.buf[..take]);
        self.buf_pos = take;
        Ok(take)
    }
}

impl std::io::BufRead for QuicRecvReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.buf_pos >= self.buf.len() {
            self.buf.clear();
            self.buf_pos = 0;
            self.buf.resize(8192, 0u8);
            let n = crate::async_rt::block_on(self.recv.read(&mut self.buf))
                .map_err(|e| std::io::Error::other(format!("quic recv: {e}")))?
                .unwrap_or(0);
            self.buf.truncate(n);
            self.bytes_read += n;
            if self.bytes_read > QUIC_STREAM_BYTE_LIMIT {
                return Err(std::io::Error::other(format!(
                    "quic recv: stream exceeded byte limit ({} bytes > {} MiB limit)",
                    self.bytes_read,
                    QUIC_STREAM_BYTE_LIMIT / (1024 * 1024),
                )));
            }
        }
        Ok(&self.buf[self.buf_pos..])
    }
    fn consume(&mut self, amt: usize) {
        self.buf_pos = (self.buf_pos + amt).min(self.buf.len());
    }
}

/// Sync wrapper around a `quinn::SendStream` that bridges async writes to the
/// synchronous `Write` trait expected by `Value::Handle`.
struct QuicSendWriter {
    send: quinn::SendStream,
}

impl std::io::Write for QuicSendWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        crate::async_rt::block_on(self.send.write_all(data))
            .map_err(|e| std::io::Error::other(format!("quic send: {e}")))?;
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(()) // quinn buffers internally; no explicit flush needed
    }
}

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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
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
        let cap_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let host_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let port_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let opts_val = args[3]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract NetCap
        let entries = match cap_val {
            Value::NetCap(e) => e,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "quic-session".to_string(),
                    "NetCap",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        let host_str = require_string("quic-session", host_val, args[1].span.clone())?;

        let port = match port_val {
            Value::Int(n) if (1..=65535).contains(&n) => n as u16,
            Value::Int(_) => {
                return Err(EvalError::user_error(
                    "quic-session: port must be 1–65535".to_string(),
                    args[2].span.clone(),
                )
                .into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "quic-session".to_string(),
                    "Int",
                    other.type_name(),
                    args[2].span.clone(),
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
        let mut tls_config = build_tls_config(&opts_val, args[3].span.clone(), &ctx).await?;

        // Override ALPN to h3 unless caller specified explicit alpn in opts.
        // build_tls_config sets alpn_protocols to ["http/1.1"] by default; replace with h3.
        // We check opts for an explicit alpn key to respect caller overrides.
        let has_explicit_alpn = matches!(&opts_val, Value::Dict(d)
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

        ok_val(Value::QuicSession(Rc::new(connection)), call_span)
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
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
/// Takes `(quic_session)`. Returns a `DatagramHandle`-like value for send/recv
/// of unreliable QUIC datagrams (RFC 9221).
///
/// TODO(http-sessions-datagram): QUIC datagrams require async send/recv via
/// `conn.send_datagram()` / `conn.read_datagram()`. The current DatagramHandle
/// uses std::net::UdpSocket (sync). Implementing QUIC datagram send/recv needs
/// either (a) a new QuicDatagramHandle variant, or (b) async wrapper types.
/// For now this returns a clear error directing users to `quic-open-stream`
/// for reliable streaming, which is the common HTTP/3 use case.
pub(crate) fn builtin_quic_open_datagram(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
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

        let session_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let conn = match session_val {
            Value::QuicSession(c) => c,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "quic-open-datagram".to_string(),
                    "QuicSession",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // Wrap the connection in a QuicDatagramHandle
        // Note: send-datagram and recv-datagram builtins must handle QuicDatagramHandle
        // separately from DatagramHandle, using async block_on(conn.send_datagram(...))
        // and block_on(conn.read_datagram(...)) respectively.
        ok_val(Value::QuicDatagramHandle(conn), call_span)
    })
}

/// `http2-session`: Establish an HTTP/2 session using reqwest.
///
/// Takes `(cap, base_url, opts)` where:
/// - `cap`: NetCap capability controlling which hosts may be contacted
/// - `base_url`: String — `scheme://host[:port]` origin (e.g. `"https://api.example.com"`)
/// - `opts`: Dict — future options (currently unused; pass `[]`)
///
/// Returns an `Http2Session` wrapping a `reqwest::Client` (async) configured
/// to prefer HTTP/2 via ALPN for HTTPS connections. The client reuses the
/// underlying connection pool across multiple `http-request` calls.
pub(crate) fn builtin_http2_session(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;

        reject_named("http2-session", named.as_ref(), call_span.clone())?;

        if args.len() != 3 {
            return Err(EvalError::user_error(
                format!(
                    "http2-session: expected 3 arguments (cap base_url opts), got {}",
                    args.len()
                ),
                call_span,
            )
            .into());
        }

        // All args pre-materialized by force_count
        let cap_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let url_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        // opts reserved for future use (ca, client cert, timeouts, etc.)
        let _opts_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Validate cap
        let entries = match cap_val {
            Value::NetCap(e) => e,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "http2-session".to_string(),
                    "NetCap",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        let base_url = require_string("http2-session", url_val, args[1].span.clone())?;

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
        // Note: opts dict is accepted but currently unused (reserved for future: mozilla-roots, ca-bundle).
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
                client: Rc::new(client),
                base_url,
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
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

        let session_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let conn = match session_val {
            Value::QuicSession(c) => c,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "http3-session".to_string(),
                    "QuicSession",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // Adapt the quinn connection into an h3-quinn connection, then build the H3 client.
        // `h3_quinn::Connection::new` takes ownership of a `quinn::Connection`.
        // We Rc::clone the connection — quinn::Connection is Clone and the clone shares
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
        // when the session is dropped (Rc refcount reaches zero).
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
            Value::Http3Session(Rc::new(RefCell::new(Http3SessionState {
                send_request,
                _driver: driver_handle,
            }))),
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
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
        let session_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let method_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let headers_val = args[3]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let body_val = args[4]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let method_str = require_string("http-request", method_val, args[1].span.clone())?;
        let path_str = require_string("http-request", path_val, args[2].span.clone())?;
        let body_str = require_string("http-request", body_val, args[4].span.clone())?;

        // Collect request headers from the Dict argument.
        // Each value is a ThunkId in the arena — resolve and materialize to extract the string.
        let req_headers: Vec<(String, String)> = match headers_val {
            Value::Dict(ref map) => {
                let mut out = Vec::with_capacity(map.len());
                for (key, val_id) in map.iter() {
                    let key_str = match key {
                        crate::value::HashableValue::Str(s) => s.to_string(),
                        crate::value::HashableValue::Int(i) => i.to_string(),
                        _ => "<other>".to_string(),
                    };
                    let thunk = ctx.thunk_arena.lock().unwrap().get(*val_id).clone();
                    let val_materialized =
                        crate::eval::materialize(&thunk, Some(&call_span), &ctx).await?;
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
                    args[3].span.clone(),
                )
                .into())
            }
        };

        match session_val {
            Value::Http3Session(session_rc) => http_request_h3(
                session_rc,
                method_str,
                path_str,
                req_headers,
                body_str,
                call_span,
                &ctx,
            ),
            Value::Http2Session { client, base_url } => {
                http_request_h2(&Http2RequestConfig {
                    client: &client,
                    base_url: &base_url,
                    method_str: &method_str,
                    path_str: &path_str,
                    req_headers: &req_headers,
                    body_str: &body_str,
                    span: call_span,
                    ctx: &ctx,
                })
                .await
            }
            other => Err(EvalError::type_mismatch_ctx(
                "http-request".to_string(),
                "Http2Session or Http3Session",
                other.type_name(),
                args[0].span.clone(),
            )
            .into()),
        }
    })
}

/// Configuration for HTTP/2 requests.
struct Http2RequestConfig<'a> {
    client: &'a Rc<reqwest::Client>,
    base_url: &'a str,
    method_str: &'a str,
    path_str: &'a str,
    req_headers: &'a [(String, String)],
    body_str: &'a str,
    span: crate::ast::Span,
    ctx: &'a crate::eval::EvalContext,
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
    let ctx = config.ctx;
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
        headers_map.insert(k, ctx.alloc_thunk(ok_val(string_val(&v), span.clone())?));
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
        ctx.alloc_thunk(ok_val(Value::Int(status), span.clone())?),
    );
    inner.insert(
        crate::value::HashableValue::Str("headers".into()),
        ctx.alloc_thunk(ok_val(Value::Dict(headers_map), span.clone())?),
    );
    inner.insert(
        crate::value::HashableValue::Str("body".into()),
        ctx.alloc_thunk(ok_val(string_val(&body_string), span.clone())?),
    );

    // Return {status: Int, headers: Dict, body: String} directly.
    // Users who want Result-based error handling wrap the call in [try [fn [] [http-request ...]]].
    ok_val(Value::Dict(inner), span)
}

/// Issue an HTTP/3 request on an existing `h3::client::SendRequest` session.
///
/// Builds the `http::Request`, sends it, collects the response headers and body,
/// and returns `{status: Int, headers: Dict, body: String}` on success or raises on error.
fn http_request_h3(
    session_rc: Rc<RefCell<crate::value::Http3SessionState>>,
    method_str: String,
    path_str: String,
    req_headers: Vec<(String, String)>,
    body_str: String,
    span: crate::ast::Span,
    ctx: &crate::eval::EvalContext,
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
    // borrow_mut() accesses send_request inside Http3SessionState.
    // Safe — single-threaded; no other borrow_mut during block_on.
    let mut stream =
        match crate::async_rt::block_on(session_rc.borrow_mut().send_request.send_request(request))
        {
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
        headers_map.insert(k, ctx.alloc_thunk(ok_val(string_val(&v), span.clone())?));
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
        ctx.alloc_thunk(ok_val(Value::Int(status), span.clone())?),
    );
    inner.insert(
        crate::value::HashableValue::Str("headers".into()),
        ctx.alloc_thunk(ok_val(Value::Dict(headers_map), span.clone())?),
    );
    inner.insert(
        crate::value::HashableValue::Str("body".into()),
        ctx.alloc_thunk(ok_val(string_val(&body_string), span.clone())?),
    );

    // Return {status: Int, headers: Dict, body: String} directly.
    // Users who want Result-based error handling wrap the call in [try [fn [] [http-request ...]]].
    ok_val(Value::Dict(inner), span)
}

/// `icmp-ping`: Send an ICMP echo request to a host.
/// Takes `(cap, host, timeout_ms)`.
/// Returns `{latency-ms: Int}` on success or raises on failure.
/// Uses unprivileged ICMP ping sockets (`SOCK_DGRAM + IPPROTO_ICMP`, Linux 3.11+).
/// Users who want Result-based error handling wrap the call in `[try [fn [] [icmp-ping ...]]]`.
pub(crate) fn builtin_icmp_ping(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
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

        let cap_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let host_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let timeout_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract NetCap entries
        let entries = match cap_val {
            Value::NetCap(e) => e,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "icmp-ping".to_string(),
                    "NetCap",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        let host = require_string("icmp-ping", host_val, args[1].span.clone())?;

        let timeout_ms = match timeout_val {
            Value::Int(n) if n >= 0 => n,
            Value::Int(_) => {
                return Err(EvalError::user_error(
                    "icmp-ping: timeout-ms must be a non-negative integer".to_string(),
                    args[2].span.clone(),
                )
                .into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "icmp-ping".to_string(),
                    "Int",
                    other.type_name(),
                    args[2].span.clone(),
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
    ctx: &crate::eval::EvalContext,
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
        ctx.alloc_thunk(ok_val(Value::Int(latency_ms), span.clone())?),
    );
    ok_val(Value::Dict(result), span)
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
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

        let data_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let handle_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract bytes to send (String or Bytes) — common to all handle variants.
        let data_bytes: Vec<u8> = match data_val {
            Value::String { source, start, end } => source[start..end].as_bytes().to_vec(),
            Value::Bytes { source, start, end } => source[start..end].to_vec(),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "send-datagram".to_string(),
                    "String or Bytes",
                    other.type_name(),
                    args[0].span.clone(),
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
            Value::QuicDatagramHandle(conn) => {
                let payload = bytes::Bytes::from(data_bytes);
                crate::async_rt::block_on(conn.send_datagram_wait(payload)).map_err(|e| {
                    EvalError::user_error(
                        format!("send-datagram: QUIC datagram send failed: {}", e),
                        call_span.clone(),
                    )
                })?;
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }

            // UDP / Unix datagram socket — synchronous send()
            Value::DatagramHandle { socket, .. } => {
                use crate::value::DatagramSocket;
                match &socket {
                    DatagramSocket::Udp(s) => s.borrow().send(&data_bytes),
                    #[cfg(unix)]
                    DatagramSocket::UnixDgram(s) => s.borrow().send(&data_bytes),
                }
                .map_err(|e| {
                    EvalError::user_error(
                        format!("send-datagram: send failed: {}", e),
                        call_span.clone(),
                    )
                })?;
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }

            other => Err(EvalError::type_mismatch_ctx(
                "send-datagram".to_string(),
                "DatagramHandle or QuicDatagramHandle",
                other.type_name(),
                args[1].span.clone(),
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
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
        let make_data_dict =
            |buf: Vec<u8>, ctx: &crate::eval::EvalContext| -> EvalResult<Arc<Thunk>> {
                let data_len = buf.len();
                let data_bytes = Value::Bytes {
                    source: Rc::from(buf.as_slice()),
                    start: 0,
                    end: data_len,
                };
                let mut dict = IndexMap::new();
                dict.insert(
                    HashableValue::Str("data".into()),
                    ctx.alloc_thunk(ok_val(data_bytes, call_span.clone())?),
                );
                ok_val(Value::Dict(dict), call_span.clone())
            };

        match val {
            // QUIC unreliable datagram (RFC 9221) — async recv via block_on.
            // `conn.read_datagram()` returns the next datagram payload as a `bytes::Bytes`.
            // Blocks until a datagram arrives or the connection closes.
            Value::QuicDatagramHandle(conn) => {
                let payload = crate::async_rt::block_on(conn.read_datagram()).map_err(|e| {
                    EvalError::user_error(
                        format!("recv-datagram: QUIC datagram recv failed: {}", e),
                        call_span.clone(),
                    )
                })?;
                make_data_dict(payload.to_vec(), &ctx)
            }

            // UDP / Unix datagram socket — synchronous recv() into a fixed-size buffer.
            // 65507 bytes is the maximum IPv4 UDP payload (65535 - 20 IP header - 8 UDP header).
            Value::DatagramHandle { socket, .. } => {
                use crate::value::DatagramSocket;
                let mut buf = vec![0u8; 65507];
                let n = match &socket {
                    DatagramSocket::Udp(s) => s.borrow().recv(&mut buf),
                    #[cfg(unix)]
                    DatagramSocket::UnixDgram(s) => s.borrow().recv(&mut buf),
                }
                .map_err(|e| {
                    EvalError::user_error(
                        format!("recv-datagram: recv failed: {}", e),
                        call_span.clone(),
                    )
                })?;
                buf.truncate(n);
                make_data_dict(buf, &ctx)
            }

            other => Err(EvalError::type_mismatch_ctx(
                "recv-datagram".to_string(),
                "DatagramHandle or QuicDatagramHandle",
                other.type_name(),
                args[0].span.clone(),
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
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
                ctx.alloc_thunk(ok_val(string_val(parsed.scheme()), call_span.clone())?),
            );

            // username (split from userinfo)
            let username = if parsed.username().is_empty() {
                Value::Dict(IndexMap::new())
            } else {
                string_val(parsed.username())
            };
            dict.insert(
                HashableValue::Str("username".into()),
                ctx.alloc_thunk(ok_val(username, call_span.clone())?),
            );

            // password (split from userinfo)
            let password = match parsed.password() {
                Some(pw) => string_val(pw),
                None => Value::Dict(IndexMap::new()),
            };
            dict.insert(
                HashableValue::Str("password".into()),
                ctx.alloc_thunk(ok_val(password, call_span.clone())?),
            );

            // host (null for non-hierarchical; strip IPv6 brackets)
            let host = match parsed.host_str() {
                Some(h) => string_val(h),
                None => Value::Dict(IndexMap::new()),
            };
            dict.insert(
                HashableValue::Str("host".into()),
                ctx.alloc_thunk(ok_val(host, call_span.clone())?),
            );

            // port (null if not specified)
            let port = match parsed.port() {
                Some(p) => Value::Int(i64::from(p)),
                None => Value::Dict(IndexMap::new()),
            };
            dict.insert(
                HashableValue::Str("port".into()),
                ctx.alloc_thunk(ok_val(port, call_span.clone())?),
            );

            // path (always present per RFC 3986)
            dict.insert(
                HashableValue::Str("path".into()),
                ctx.alloc_thunk(ok_val(string_val(parsed.path()), call_span.clone())?),
            );

            // query (null if absent)
            let query = match parsed.query() {
                Some(q) => string_val(q),
                None => Value::Dict(IndexMap::new()),
            };
            dict.insert(
                HashableValue::Str("query".into()),
                ctx.alloc_thunk(ok_val(query, call_span.clone())?),
            );

            // fragment (null if absent)
            let fragment = match parsed.fragment() {
                Some(f) => string_val(f),
                None => Value::Dict(IndexMap::new()),
            };
            dict.insert(
                HashableValue::Str("fragment".into()),
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
            HashableValue::Str("scheme".into()),
            ctx.alloc_thunk(ok_val(
                string_val(&scheme.to_lowercase()),
                call_span.clone(),
            )?),
        );

        // Non-hierarchical URIs: all null for userinfo/host/port
        for key in ["username", "password", "host", "port"] {
            dict.insert(
                HashableValue::Str(key.into()),
                ctx.alloc_thunk(ok_val(Value::Dict(IndexMap::new()), call_span.clone())?),
            );
        }

        // path is the remaining part after scheme:
        // For mailto:user@example.com, path is "user@example.com"
        // For urn:isbn:123, path is "isbn:123"
        dict.insert(
            HashableValue::Str("path".into()),
            ctx.alloc_thunk(ok_val(string_val(rest), call_span.clone())?),
        );

        // query and fragment: null (non-hierarchical URIs typically don't have these)
        for key in ["query", "fragment"] {
            dict.insert(
                HashableValue::Str(key.into()),
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
            ..
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
            HashableValue::Str("scheme".into()),
            ctx.alloc_thunk(ok_val(string_val(parsed.scheme()), call_span.clone())?),
        );

        // username (split from userinfo)
        let username = if parsed.username().is_empty() {
            Value::Dict(IndexMap::new())
        } else {
            string_val(parsed.username())
        };
        dict.insert(
            HashableValue::Str("username".into()),
            ctx.alloc_thunk(ok_val(username, call_span.clone())?),
        );

        // password (split from userinfo)
        let password = match parsed.password() {
            Some(pw) => string_val(pw),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            HashableValue::Str("password".into()),
            ctx.alloc_thunk(ok_val(password, call_span.clone())?),
        );

        // host (always present for URLs; unwrap is safe)
        dict.insert(
            HashableValue::Str("host".into()),
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
            HashableValue::Str("port".into()),
            ctx.alloc_thunk(ok_val(Value::Int(i64::from(port)), call_span.clone())?),
        );

        // path (always present per RFC 3986; default to "/" if empty)
        let path = if parsed.path().is_empty() {
            "/"
        } else {
            parsed.path()
        };
        dict.insert(
            HashableValue::Str("path".into()),
            ctx.alloc_thunk(ok_val(string_val(path), call_span.clone())?),
        );

        // query (null if absent)
        let query = match parsed.query() {
            Some(q) => string_val(q),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            HashableValue::Str("query".into()),
            ctx.alloc_thunk(ok_val(query, call_span.clone())?),
        );

        // fragment (null if absent)
        let fragment = match parsed.fragment() {
            Some(f) => string_val(f),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            HashableValue::Str("fragment".into()),
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
            ..
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
            HashableValue::Str("nid".into()),
            ctx.alloc_thunk(ok_val(string_val(nid), call_span.clone())?),
        );
        dict.insert(
            HashableValue::Str("nss".into()),
            ctx.alloc_thunk(ok_val(string_val(nss), call_span.clone())?),
        );

        // r-component (null if absent)
        let r_val = match r_component {
            Some(r) => string_val(r),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            HashableValue::Str("r-component".into()),
            ctx.alloc_thunk(ok_val(r_val, call_span.clone())?),
        );

        // q-component (null if absent)
        let q_val = match q_component {
            Some(q) => string_val(q),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            HashableValue::Str("q-component".into()),
            ctx.alloc_thunk(ok_val(q_val, call_span.clone())?),
        );

        // fragment (null if absent)
        let frag_val = match fragment {
            Some(f) => string_val(f),
            None => Value::Dict(IndexMap::new()),
        };
        dict.insert(
            HashableValue::Str("fragment".into()),
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
        let mut fields = indexmap::IndexMap::new();
        fields.insert(
            format!("__cap_flag_{}", flag_name.to_lowercase()),
            Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            }),
        );
        Type::Record(Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        })
    }

    // ── Type aliases ──────────────────────────────────────────────────────────
    // Register so @QuicSession, @Http2Session, etc. are valid in user annotations.

    for (name, body) in [
        ("QuicSession", Type::QuicSession),
        ("Http2Session", Type::Http2Session),
        ("Http3Session", Type::Http3Session),
        ("QuicDatagramHandle", Type::QuicDatagramHandle),
        ("DatagramHandle", Type::DatagramHandle),
        ("Url", Type::Uri),
    ] {
        env.insert_tycon_def(
            name.to_string(),
            Arc::new(TyConDef {
                params: vec![],
                body,
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: None,
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
            }),
        );
    }

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
            ret: Box::new(Type::handle(cap_flag("readable"))),
            variadic: false,
            required_count: 4,
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
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ), // opts dict: no required fields (BAS width subtyping)
                (None, Type::handle(cap_flag("readable"))), // handle
            ],
            ret: Box::new(Type::handle(cap_flag("readable"))),
            variadic: false,
            required_count: 3,
        },
    );

    // ── tls-peer-cert: Handle → Dict ─────────────────────────────────────────
    // Extracts TLS certificate metadata from a TLS handle.
    env.insert(
        "builtin-tls-peer-cert".to_string(),
        Type::Function {
            params: vec![(None, Type::handle(cap_flag("readable")))],
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::from_iter([
                    ("subject".to_string(), Type::Str),
                    ("issuer".to_string(), Type::Str),
                    (
                        "sans".to_string(),
                        Type::App(Box::new(Type::TyCon("Seq".into())), Box::new(Type::Str)),
                    ),
                    ("not-before".to_string(), Type::Int),
                    ("not-after".to_string(), Type::Int),
                    ("spki-sha256".to_string(), Type::Str),
                ]),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 1,
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
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 2,
        },
    );

    // ── recv-datagram: DatagramHandle → {data: Bytes, addr: Str, port: Int} ──
    env.insert(
        "builtin-recv-datagram".to_string(),
        Type::Function {
            params: vec![(
                None,
                Type::normalize_union(vec![Type::DatagramHandle, Type::QuicDatagramHandle]),
            )],
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::from_iter([
                    ("data".to_string(), Type::Bytes),
                    ("addr".to_string(), Type::Str),
                    ("port".to_string(), Type::Int),
                ]),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 1,
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
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ), // opts dict (TLS options; no required fields)
            ],
            ret: Box::new(Type::QuicSession),
            variadic: false,
            required_count: 4,
        },
    );

    // ── quic-open-stream: QuicSession → Handle[Readable Writable Binary Stream] ──
    env.insert(
        "quic-open-stream".to_string(),
        Type::Function {
            params: vec![(None, Type::QuicSession)],
            ret: Box::new(Type::handle(cap_flag("readable"))),
            variadic: false,
            required_count: 1,
        },
    );

    // ── quic-open-datagram: QuicSession → QuicDatagramHandle ─────────────────
    env.insert(
        "quic-open-datagram".to_string(),
        Type::Function {
            params: vec![(None, Type::QuicSession)],
            ret: Box::new(Type::QuicDatagramHandle),
            variadic: false,
            required_count: 1,
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
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ), // opts dict (reserved; no required fields)
            ],
            ret: Box::new(Type::Http2Session),
            variadic: false,
            required_count: 3,
        },
    );

    // ── http3-session: QuicSession → Http3Session ─────────────────────────────
    env.insert(
        "http3-session".to_string(),
        Type::Function {
            params: vec![(None, Type::QuicSession)],
            ret: Box::new(Type::Http3Session),
            variadic: false,
            required_count: 1,
        },
    );

    // ── http-request: (Http2Session | Http3Session) → String → String → Dict → String → Result ──
    // Returns {ok: {status: Int, headers: Dict, body: Str}} or {err: msg} — direct Result, no try needed.
    // Returns Top since Result variant is nominal.
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
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ), // headers dict (any dict; BAS width subtyping)
                (None, Type::Str), // body: runtime calls require_string — Bytes not accepted
            ],
            // Returns {ok: {status headers body}} or {err: msg} — Top since Result variant is nominal.
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 5,
        },
    );

    // ── icmp-ping: NetCap → String → Int → Dict ───────────────────────────────
    // Returns {ok: {latency-ms: Int}} or {err: Str} via builtin-try.
    env.insert(
        "icmp-ping".to_string(),
        Type::Function {
            params: vec![
                (None, Type::NetCap),
                (None, Type::Str), // host
                (None, Type::Int), // timeout_ms
            ],
            ret: Box::new(Type::normalize_union(vec![
                Type::Record(Row {
                    fields: indexmap::IndexMap::from_iter([(
                        "ok".to_string(),
                        Type::Record(Row {
                            fields: indexmap::IndexMap::from_iter([(
                                "latency-ms".to_string(),
                                Type::Int,
                            )]),
                            tail: crate::type_def::RowTail::Empty,
                        }),
                    )]),
                    tail: crate::type_def::RowTail::Empty,
                }),
                Type::Record(Row {
                    fields: indexmap::IndexMap::from_iter([("err".to_string(), Type::Str)]),
                    tail: crate::type_def::RowTail::Empty,
                }),
            ])),
            variadic: false,
            required_count: 3,
        },
    );

    // ── uri: String → Uri ─────────────────────────────────────────────────────
    env.insert(
        "uri".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Uri),
            variadic: false,
            required_count: 1,
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
            required_count: 1,
        },
    );

    // ── urn: String → Uri ─────────────────────────────────────────────────────
    env.insert(
        "urn".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Uri),
            variadic: false,
            required_count: 1,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Span;
    use crate::rust_span;
    use crate::value::NetCapEntry;

    fn dummy_span() -> Span {
        rust_span!()
    }

    #[test]
    fn test_check_net_cap_allowlist_denial() {
        // Allowlist: only api.example.com:443 is allowed.
        let entries = vec![NetCapEntry::HostPort("api.example.com".to_string(), 443)];
        let span = dummy_span();

        // Allowed host:port → Ok
        let result = check_net_cap_allowlist(&entries, "api.example.com", Some(443), span.clone());
        assert!(
            result.is_ok(),
            "api.example.com:443 should be allowed, got: {:?}",
            result
        );

        // Denied host (different hostname, same port) → Err
        let result = check_net_cap_allowlist(&entries, "evil.example.com", Some(443), span.clone());
        assert!(result.is_err(), "evil.example.com:443 should be denied");
        let msg = result.unwrap_err().kind.to_string().to_string();
        assert!(
            msg.contains("denied"),
            "error should mention 'denied', got: {msg}"
        );

        // Denied port (correct host, wrong port) → Err
        let result = check_net_cap_allowlist(&entries, "api.example.com", Some(80), span.clone());
        assert!(
            result.is_err(),
            "api.example.com:80 should be denied (only port 443 is allowed)"
        );

        // Any allowlist → allows everything
        let any_entries = vec![NetCapEntry::Any];
        let result = check_net_cap_allowlist(
            &any_entries,
            "anything.example.com",
            Some(1234),
            span.clone(),
        );
        assert!(
            result.is_ok(),
            "NetCapEntry::Any should allow any host:port"
        );
        // Any also allows hosts not in the original restricted list
        let result = check_net_cap_allowlist(&any_entries, "evil.example.com", Some(22), span);
        assert!(
            result.is_ok(),
            "NetCapEntry::Any should allow evil.example.com:22"
        );
    }
}
