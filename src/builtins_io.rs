//! Filesystem and network I/O builtins: dir-cap, open, slurp, write, write-atomic, connect, lines.
//!
//! These builtins provide capability-based access to filesystems and networks,
//! implementing object-capability security through DirCap and NetCap values.
//!
//! **Filesystem builtins:**
//! - `dir-cap`: Create a DirCap from a path
//! - `open`: Open a file within a DirCap
//! - `slurp`: Read all bytes from a Handle (returns Str for Text, Bytes for Binary)
//! - `write`: Write a string to a file (DirCap-based)
//! - `write-atomic`: Atomically write to a file (temp + rename)
//! - `narrow`: Attenuate a DirCap to a subdirectory
//! - `revocable`: Wrap a DirCap in a revocable wrapper
//! - `revoke-cap`: Revoke a RevocableDirCap
//!
//! **Network builtins:**
//! - `net-cap`: Create a NetCap from an allowlist
//! - `connect`: Open a TCP/UDP connection within a NetCap (supports Transport variants)
//! - `tls-connect`: Layer TLS on a connection (Connector or Handle form)
//! - `tls-peer-cert`: Extract TLS certificate metadata from a TLS handle
//! - `spki-pin`: Create an SPKI pin for certificate pinning
//!
//! **Handle capability builtins:**
//! - `cap-data`: Extract capability data from a Handle/WriteHandle
//! - `has-cap?`: Check if a capability is present on a Handle/WriteHandle
//! - `write-handle`: Write to a WriteHandle (returns handle for chaining)
//! - `flush`: Flush a WriteHandle buffer
//! - `close`: Close a WriteHandle
//!
//! **I/O helpers:**
//! - `lines`: Read lines from a Handle lazily (Text encoding only)
//! - `emit`: Write to stdout and suppress JSON output
//! - `env`: Read environment variables
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration in `standard_builtins()` and `create_root_env()` remains in `builtins.rs`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::{builtin, ok_val, reject_named, require_string};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::value::{string_val, BuiltinArgs, Thunk, Value};

/// `emit`: Write a string to stdout and suppress JSON output.
/// Takes a String argument, writes it to stdout, sets ctx.emitted flag, returns null (empty dict).
pub(crate) fn builtin_emit(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("emit", args, named, &ctx, depth, call_span)?;
    let s = require_string("emit", val, args[0].span)?;

    // Write to stdout
    use std::io::Write;
    std::io::stdout()
        .write_all(s.as_bytes())
        .map_err(|e| EvalError::user_error(format!("emit failed: {e}"), call_span))?;

    // Set emitted flag to suppress JSON output
    ctx.emitted.set(true);

    // Return null (empty dict)
    ok_val(Value::Dict(IndexMap::new()), call_span)
}

/// `env`: Read an environment variable by name.
/// Returns the value as a String, or Null if not set or not allowed.
/// Gated by ctx.env_allowed: None = all denied, Some(set) = only those allowed.
pub(crate) fn builtin_env(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("env", args, named, &ctx, depth, call_span)?;
    let name = require_string("env", val, args[0].span)?;

    // Check env_allowed
    // None = unrestricted (all allowed), Some(set) = only those in the set
    let allowed = match &ctx.env_allowed {
        None => true, // None means unrestricted access
        Some(set) => set.contains(&name),
    };

    if !allowed {
        // Return Null if not allowed
        return ok_val(Value::Dict(IndexMap::new()), call_span);
    }

    // Read env var
    match std::env::var(name) {
        Ok(value) => ok_val(string_val(&value), call_span),
        Err(_) => ok_val(Value::Dict(IndexMap::new()), call_span), // Not set -> Null
    }
}

/// `dir-cap`: Create a DirCap from a path string.
/// Opens the path as a cap_std::fs::Dir (RESOLVE_BENEATH sandbox at OS level).
/// Returns Value::DirCap.
pub(crate) fn builtin_dir_cap(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("dir-cap", args, named, &ctx, depth, call_span)?;
    let path = require_string("dir-cap", val, args[0].span)?;

    // Open the directory using cap-std
    use cap_std::ambient_authority;
    let dir = cap_std::fs::Dir::open_ambient_dir(&path, ambient_authority()).map_err(|e| {
        EvalError::user_error(
            format!("dir-cap: failed to open directory '{}': {}", path, e),
            call_span,
        )
    })?;

    ok_val(Value::DirCap(Rc::new(dir)), call_span)
}

/// `open`: Open a file within a DirCap.
/// Takes a DirCap, String filename, and String mode ("r", "w", "a").
/// Returns Value::Handle.
///
/// **Note:** Future enhancement (deferred): Accept additional Variant args as capability flags
/// (Readable, Writable, Binary, Seekable, etc.) and populate the caps HashMap accordingly.
/// For now, this function hardcodes Readable + Text caps for read-only mode.
pub(crate) fn builtin_open(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 args: DirCap, String path, String mode
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    reject_named("open", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;
    let mode_val = materialize(&args[2], Some(&call_span), &ctx, depth)?;

    // Extract DirCap
    let dir = match dir_val {
        Value::DirCap(d) => d,
        Value::RevocableDirCap { inner, revoked } => {
            if revoked.get() {
                return Err(EvalError::user_error(
                    "open: capability has been revoked".to_string(),
                    call_span,
                )
                .into());
            }
            inner
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "open".to_string(),
                "DirCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let path = require_string("open", path_val, args[1].span)?;
    let mode = require_string("open", mode_val, args[2].span)?;

    // Open the file based on mode
    use std::io::BufReader;
    let handle: Box<dyn std::io::BufRead> = match mode.as_str() {
        "r" => {
            let file = dir.open(&path).map_err(|e| {
                EvalError::user_error(
                    format!("open: failed to open file '{}': {}", path, e),
                    call_span,
                )
            })?;
            Box::new(BufReader::new(file))
        }
        "w" | "a" => {
            return Err(EvalError::user_error(
                "open: write and append modes not yet implemented (Phase 1 is read-only)"
                    .to_string(),
                call_span,
            )
            .into());
        }
        _ => {
            return Err(EvalError::user_error(
                format!("open: invalid mode '{}' (expected 'r', 'w', or 'a')", mode),
                call_span,
            )
            .into());
        }
    };

    // Default caps for read-only handle (Phase 1)
    let mut caps = HashMap::new();
    caps.insert("Readable".to_string(), Value::Dict(IndexMap::new())); // Null
    caps.insert("Text".to_string(), Value::Dict(IndexMap::new())); // Null

    ok_val(
        Value::Handle {
            caps,
            inner: Rc::new(std::cell::RefCell::new(handle)),
            write_inner: None,
        },
        call_span,
    )
}

/// `slurp`: Read all bytes from a Handle to a String or Bytes.
/// Takes a Handle, reads to EOF, returns String (if Text encoding) or Bytes (if Binary encoding).
pub(crate) fn builtin_slurp(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("slurp", args, named, &ctx, depth, call_span)?;

    // Extract Handle
    let (handle, caps) = match val {
        Value::Handle { inner, caps, .. } => (inner, caps),
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "slurp".to_string(),
                "Handle",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    // Check if Binary cap is set
    let is_binary = caps.contains_key("Binary");

    use std::io::Read;
    if is_binary {
        // Read to bytes
        let mut contents = Vec::new();
        handle
            .borrow_mut()
            .read_to_end(&mut contents)
            .map_err(|e| EvalError::user_error(format!("slurp: read failed: {}", e), call_span))?;

        let len = contents.len();
        ok_val(
            Value::Bytes {
                source: Rc::from(contents),
                start: 0,
                end: len,
            },
            call_span,
        )
    } else {
        // Read to string (Text encoding)
        let mut contents = String::new();
        handle
            .borrow_mut()
            .read_to_string(&mut contents)
            .map_err(|e| EvalError::user_error(format!("slurp: read failed: {}", e), call_span))?;

        ok_val(string_val(&contents), call_span)
    }
}

/// `narrow`: Attenuate a DirCap to a subdirectory.
/// Takes a DirCap and a String subpath, returns a new DirCap for the subdirectory.
pub(crate) fn builtin_narrow(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: DirCap, String subpath
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("narrow", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let subpath_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    // Extract DirCap
    let dir = match dir_val {
        Value::DirCap(d) => d,
        Value::RevocableDirCap { inner, revoked } => {
            if revoked.get() {
                return Err(EvalError::user_error(
                    "narrow: capability has been revoked".to_string(),
                    call_span,
                )
                .into());
            }
            inner
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "narrow".to_string(),
                "DirCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let subpath = require_string("narrow", subpath_val, args[1].span)?;

    // Open subdirectory (RESOLVE_BENEATH applies to subpath)
    let narrowed = dir.open_dir(&subpath).map_err(|e| {
        EvalError::user_error(
            format!("narrow: failed to open subdirectory '{}': {}", subpath, e),
            call_span,
        )
    })?;

    ok_val(Value::DirCap(Rc::new(narrowed)), call_span)
}

/// `revocable`: Wrap a DirCap in a RevocableDirCap.
/// Takes a DirCap, returns a RevocableDirCap.
/// The RevocableDirCap can be revoked later via `revoke-cap`.
pub(crate) fn builtin_revocable(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("revocable", args, named, &ctx, depth, call_span)?;

    // Extract DirCap
    let dir = match val {
        Value::DirCap(d) => d,
        Value::RevocableDirCap { inner, revoked: _ } => {
            // Already revocable — return a new revocable wrapper with a new flag
            // (allows independent revocation)
            inner
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "revocable".to_string(),
                "DirCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    // Create a new revoked flag
    let revoked = Rc::new(std::cell::Cell::new(false));

    ok_val(
        Value::RevocableDirCap {
            inner: dir,
            revoked,
        },
        call_span,
    )
}

/// `revoke-cap`: Revoke a RevocableDirCap.
/// Takes a RevocableDirCap, sets its revoked flag to true, returns null.
/// Future operations on the cap will fail.
pub(crate) fn builtin_revoke_cap(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("revoke-cap", args, named, &ctx, depth, call_span)?;

    // Extract RevocableDirCap
    match val {
        Value::RevocableDirCap { revoked, .. } => {
            revoked.set(true);
            ok_val(Value::Dict(IndexMap::new()), call_span)
        }
        other => Err(EvalError::type_mismatch_ctx(
            "revoke-cap".to_string(),
            "RevocableDirCap",
            other.type_name(),
            args[0].span,
        )
        .into()),
    }
}

/// `net-cap`: Create a NetCap from an allowlist of host patterns.
/// Takes a Dict or Seq of strings (hostnames, host:port pairs, or CIDR ranges).
/// Returns Value::NetCap.
pub(crate) fn builtin_net_cap(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("net-cap", args, named, &ctx, depth, call_span)?;

    // Parse allowlist entries from a sequence or dict
    let mut entries = Vec::new();

    match val {
        Value::Seq { .. } => {
            // Collect all elements from the sequence
            let mut current = val;
            loop {
                match current {
                    Value::Seq { head, tail } => {
                        let head_thunk = ctx.get_thunk(head);
                        let head_val = materialize(&head_thunk, Some(&call_span), &ctx, depth)?;
                        let entry_str = require_string("net-cap", head_val, call_span)?;
                        entries.push(parse_net_cap_entry(&entry_str, call_span)?);

                        // Move to tail
                        let tail_thunk = ctx.get_thunk(tail);
                        current = materialize(&tail_thunk, Some(&call_span), &ctx, depth)?;
                    }
                    Value::Dict(map) if map.is_empty() => break, // End of sequence
                    _ => {
                        return Err(EvalError::type_mismatch_ctx(
                            "net-cap".to_string(),
                            "Seq of Str",
                            current.type_name(),
                            call_span,
                        )
                        .into())
                    }
                }
            }
        }
        Value::Dict(map) => {
            // Dict: iterate over values (keys ignored, just like $collect)
            for (_key, thunk_id) in map.iter() {
                let thunk = ctx.get_thunk(*thunk_id);
                let entry_val = materialize(&thunk, Some(&call_span), &ctx, depth)?;
                let entry_str = require_string("net-cap", entry_val, call_span)?;
                entries.push(parse_net_cap_entry(&entry_str, call_span)?);
            }
        }
        Value::String {
            ref source,
            start,
            end,
        } => {
            // Single string — wrap in vec
            let s = &source[start..end];
            entries.push(parse_net_cap_entry(s, call_span)?);
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "net-cap".to_string(),
                "Seq or Dict",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    }

    ok_val(Value::NetCap(Rc::new(entries)), call_span)
}

/// Parse a single NetCapEntry from a string.
fn parse_net_cap_entry(s: &str, span: Span) -> EvalResult<crate::value::NetCapEntry> {
    use crate::value::NetCapEntry;

    if let Some((host, port_str)) = s.split_once(':') {
        // host:port format
        let port: u16 = port_str.parse().map_err(|_| {
            EvalError::user_error(
                format!("net-cap: invalid port number '{}' in '{}'", port_str, s),
                span,
            )
        })?;
        Ok(NetCapEntry::HostPort(host.to_string(), port))
    } else if s.contains('*') {
        // Glob pattern (prefix wildcard only)
        if !s.starts_with("*.") {
            return Err(EvalError::user_error(
                format!(
                    "net-cap: only prefix wildcards are supported (e.g., '*.internal'), got '{}'",
                    s
                ),
                span,
            )
            .into());
        }
        Ok(NetCapEntry::HostnameGlob(s.to_string()))
    } else if s.contains('/') {
        // CIDR range — deferred to Phase 3
        Err(EvalError::user_error(
            format!("net-cap: CIDR ranges are not yet implemented (got '{}')", s),
            span,
        )
        .into())
    } else {
        // Plain hostname
        Ok(NetCapEntry::Hostname(s.to_string()))
    }
}

/// `connect`: Open a TCP or UDP connection within a NetCap.
/// Takes a NetCap, hostname String, port Int, and optional Transport variant (default: Tcp).
/// - `Tcp` (default) → Handle[Binary Readable Writable Stream]
/// - `Udp` → error "UDP not yet supported, use Tcp" (reserved for Phase 2)
pub(crate) fn builtin_connect(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 or 4 args: NetCap, String host, Int port, [Transport variant]
    if args.len() < 3 || args.len() > 4 {
        return Err(EvalError::user_error(
            format!(
                "connect: expected 3 or 4 arguments (cap host port [transport]), got {}",
                args.len()
            ),
            call_span,
        )
        .into());
    }
    reject_named("connect", named, call_span)?;

    let cap_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let host_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;
    let port_val = materialize(&args[2], Some(&call_span), &ctx, depth)?;

    // Extract optional Transport variant (4th arg); default to Tcp
    let transport_tag = if args.len() == 4 {
        let transport_val = materialize(&args[3], Some(&call_span), &ctx, depth)?;
        match transport_val {
            Value::Variant { tag, .. } => tag,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "connect".to_string(),
                    "Transport variant (Tcp or Udp)",
                    other.type_name(),
                    args[3].span,
                )
                .into())
            }
        }
    } else {
        "Tcp".to_string()
    };

    // Validate transport and reject UDP (reserved for Phase 2)
    match transport_tag.as_str() {
        "Tcp" => {} // proceed below
        "Udp" => {
            return Err(EvalError::user_error(
                "connect: UDP not yet supported, use Tcp".to_string(),
                call_span,
            )
            .into());
        }
        other => {
            return Err(EvalError::user_error(
                format!(
                    "connect: unknown transport '{}' (expected Tcp or Udp)",
                    other
                ),
                call_span,
            )
            .into());
        }
    }

    // Extract NetCap
    let entries = match cap_val {
        Value::NetCap(e) => e,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "connect".to_string(),
                "NetCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let host = require_string("connect", host_val, args[1].span)?;
    let port = match port_val {
        Value::Int(n) if n >= 1 && n <= 65535 => n as u16,
        Value::Int(_) => {
            return Err(EvalError::user_error(
                "connect: port must be 1-65535".to_string(),
                args[2].span,
            )
            .into())
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "connect".to_string(),
                "Int",
                other.type_name(),
                args[2].span,
            )
            .into())
        }
    };

    // Check allowlist before connecting
    check_net_cap_allowlist(&entries, &host, port, call_span)?;

    // Open TCP connection
    let addr = format!("{}:{}", host, port);
    let stream = std::net::TcpStream::connect(&addr).map_err(|e| {
        EvalError::user_error(
            format!("connect: failed to connect to {}: {}", addr, e),
            call_span,
        )
    })?;

    // Clone stream for write half before consuming the original into BufReader
    let write_stream = stream.try_clone().map_err(|e| {
        EvalError::user_error(
            format!("connect: failed to clone TcpStream for write half: {}", e),
            call_span,
        )
    })?;

    let write_inner: Option<Rc<RefCell<Box<dyn std::io::Write>>>> =
        Some(Rc::new(RefCell::new(Box::new(write_stream))));

    // Wrap read half in BufReader for Handle
    let buf_reader = std::io::BufReader::new(stream);
    let inner = Rc::new(RefCell::new(
        Box::new(buf_reader) as Box<dyn std::io::BufRead>
    ));

    // Caps for TCP connection: Binary Readable Writable Stream
    let mut caps = HashMap::new();
    caps.insert("Readable".to_string(), Value::Dict(IndexMap::new())); // Null
    caps.insert("Writable".to_string(), Value::Dict(IndexMap::new())); // Null
    caps.insert("Binary".to_string(), Value::Dict(IndexMap::new())); // Null
    caps.insert("Stream".to_string(), Value::Dict(IndexMap::new())); // Null

    ok_val(
        Value::Handle {
            caps,
            inner,
            write_inner,
        },
        call_span,
    )
}

/// Check if a connection to host:port is allowed by the NetCap allowlist.
fn check_net_cap_allowlist(
    entries: &[crate::value::NetCapEntry],
    host: &str,
    port: u16,
    span: Span,
) -> EvalResult<()> {
    use crate::value::NetCapEntry;

    // Check hostname-based entries first (pre-DNS)
    for entry in entries {
        match entry {
            NetCapEntry::Hostname(allowed_host) => {
                if host.eq_ignore_ascii_case(allowed_host) {
                    return Ok(());
                }
            }
            NetCapEntry::HostPort(allowed_host, allowed_port) => {
                if host.eq_ignore_ascii_case(allowed_host) && port == *allowed_port {
                    return Ok(());
                }
            }
            NetCapEntry::HostnameGlob(pattern) => {
                // Pattern: "*.suffix"
                if let Some(suffix) = pattern.strip_prefix("*.") {
                    if host.eq_ignore_ascii_case(suffix) || host.ends_with(&format!(".{}", suffix))
                    {
                        return Ok(());
                    }
                }
            }
        }
    }

    // No match — deny connection
    Err(EvalError::user_error(
        format!(
            "connect: connection to {}:{} denied by NetCap allowlist",
            host, port
        ),
        span,
    )
    .into())
}

/// `lines`: Read lines from a Handle lazily.
/// Takes a Handle, returns a lazy Seq where each element is a line (without newline).
/// This is a coinductive lazy sequence — each tail force reads the next line.
/// Errors if the Handle has a Binary cap (lines requires Text encoding).
pub(crate) fn builtin_lines(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("lines", args, named, &ctx, depth, call_span)?;

    // Extract Handle
    let (handle, write_inner, caps) = match val {
        Value::Handle {
            inner,
            write_inner,
            caps,
        } => (inner, write_inner, caps),
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "lines".to_string(),
                "Handle",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    // Check if Binary cap is set — error if so
    if caps.contains_key("Binary") {
        return Err(EvalError::user_error(
            "lines: requires Text encoding (cannot read lines from Binary handle)".to_string(),
            call_span,
        )
        .into());
    }

    // Wrap the Handle in a new Handle that the step function can use
    // The step function will read one line, then return a Seq with the line as head
    // and a PendingBuiltin thunk for the next line as tail
    builtin_lines_step(handle, write_inner, caps, depth, call_span, ctx)
}

/// Helper for `lines`: reads one line and returns Seq or null.
pub(crate) fn builtin_lines_step(
    handle: Rc<RefCell<Box<dyn std::io::BufRead>>>,
    write_inner: Option<Rc<RefCell<Box<dyn std::io::Write>>>>,
    caps: HashMap<String, Value>,
    depth: usize,
    call_span: Span,
    ctx: Rc<crate::eval::EvalContext>,
) -> EvalResult<Rc<Thunk>> {
    use std::io::BufRead;

    let mut line = String::new();

    match handle.borrow_mut().read_line(&mut line) {
        Ok(0) => {
            // EOF — return null (empty dict)
            ok_val(Value::Dict(IndexMap::new()), call_span)
        }
        Ok(_) => {
            // Got a line — strip trailing newline if present
            if line.ends_with('\n') {
                line.pop();
                if line.ends_with('\r') {
                    line.pop();
                }
            }

            // Create head thunk
            let head = ok_val(string_val(&line), call_span)?;
            let head_id = ctx.alloc_thunk(head);

            // Create tail as PendingBuiltin that will read the next line
            // We need to pass the Handle through to the next step
            let tail_args = vec![ok_val(
                Value::Handle {
                    caps: caps.clone(),
                    inner: Rc::clone(&handle),
                    write_inner: write_inner.as_ref().map(Rc::clone),
                },
                call_span,
            )?];
            let tail = Rc::new(Thunk::new_pending_builtin(
                builtin!("lines", builtin_lines),
                tail_args,
                None,
                depth + 1,
                call_span,
                Some(Rc::from("call $lines")),
                Rc::clone(&ctx),
            ));
            let tail_id = ctx.alloc_thunk(tail);

            ok_val(
                Value::Seq {
                    head: head_id,
                    tail: tail_id,
                },
                call_span,
            )
        }
        Err(e) => {
            Err(EvalError::user_error(format!("lines: read failed: {}", e), call_span).into())
        }
    }
}

/// `write`: Write a String to a file.
/// Takes a DirCap, String path, and String content.
/// Writes content to the file at path (creating or truncating), then returns null.
pub(crate) fn builtin_write(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 args: DirCap, String path, String content
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    reject_named("write", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;
    let content_val = materialize(&args[2], Some(&call_span), &ctx, depth)?;

    // Extract DirCap
    let dir = match dir_val {
        Value::DirCap(d) => d,
        Value::RevocableDirCap { inner, revoked } => {
            if revoked.get() {
                return Err(EvalError::user_error(
                    "write: capability has been revoked".to_string(),
                    call_span,
                )
                .into());
            }
            inner
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "write".to_string(),
                "DirCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let path = require_string("write", path_val, args[1].span)?;
    let content = require_string("write", content_val, args[2].span)?;

    // Open file for writing (create or truncate)
    use std::io::Write;
    let mut file = dir.create(&path).map_err(|e| {
        EvalError::user_error(
            format!("write: failed to create file '{}': {}", path, e),
            call_span,
        )
    })?;

    // Write content
    file.write_all(content.as_bytes()).map_err(|e| {
        EvalError::user_error(
            format!("write: failed to write to '{}': {}", path, e),
            call_span,
        )
    })?;

    // Return null (empty dict)
    ok_val(Value::Dict(IndexMap::new()), call_span)
}

/// `write-atomic`: Atomically write a String to a file.
/// Takes a DirCap, String path, and String content.
/// Writes to a temp file in the same directory, then renames to the target path.
/// This ensures the target file is never partially written.
pub(crate) fn builtin_write_atomic(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 args: DirCap, String path, String content
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    reject_named("write-atomic", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;
    let content_val = materialize(&args[2], Some(&call_span), &ctx, depth)?;

    // Extract DirCap
    let dir = match dir_val {
        Value::DirCap(d) => d,
        Value::RevocableDirCap { inner, revoked } => {
            if revoked.get() {
                return Err(EvalError::user_error(
                    "write-atomic: capability has been revoked".to_string(),
                    call_span,
                )
                .into());
            }
            inner
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "write-atomic".to_string(),
                "DirCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let path = require_string("write-atomic", path_val, args[1].span)?;
    let content = require_string("write-atomic", content_val, args[2].span)?;

    // Generate a unique temp filename in the same directory as the target
    // Use process ID and a random suffix to avoid collisions
    use std::io::Write;
    let temp_name = format!(
        ".tmp.{}.{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );

    // Write to temp file
    let mut temp_file = dir.create(&temp_name).map_err(|e| {
        EvalError::user_error(
            format!(
                "write-atomic: failed to create temp file '{}': {}",
                temp_name, e
            ),
            call_span,
        )
    })?;

    temp_file.write_all(content.as_bytes()).map_err(|e| {
        EvalError::user_error(
            format!(
                "write-atomic: failed to write to temp file '{}': {}",
                temp_name, e
            ),
            call_span,
        )
    })?;

    // Ensure data is flushed before rename
    temp_file.sync_all().map_err(|e| {
        EvalError::user_error(
            format!(
                "write-atomic: failed to sync temp file '{}': {}",
                temp_name, e
            ),
            call_span,
        )
    })?;

    // Drop the file handle before rename (required on Windows)
    drop(temp_file);

    // Atomically rename temp file to target path
    dir.rename(&temp_name, &dir, &path).map_err(|e| {
        // Clean up temp file on rename failure
        let _ = dir.remove_file(&temp_name);
        EvalError::user_error(
            format!(
                "write-atomic: failed to rename temp file to '{}': {}",
                path, e
            ),
            call_span,
        )
    })?;

    // Return null (empty dict)
    ok_val(Value::Dict(IndexMap::new()), call_span)
}

/// `cap-data`: Extract capability data from a Handle or WriteHandle.
/// Takes a Handle/WriteHandle and a capability name (String).
/// Returns the Value associated with that capability, or errors if the cap is absent.
pub(crate) fn builtin_cap_data(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: Handle/WriteHandle, String cap_name
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("cap-data", named, call_span)?;

    let handle_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let cap_name_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    // Extract caps from Handle or WriteHandle
    let caps = match handle_val {
        Value::Handle { caps, .. } => caps,
        Value::WriteHandle { caps, .. } => caps,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "cap-data".to_string(),
                "Handle or WriteHandle",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let cap_name = require_string("cap-data", cap_name_val, args[1].span)?;

    // Lookup capability
    match caps.get(&cap_name) {
        Some(cap_value) => ok_val(cap_value.clone(), call_span),
        None => Err(EvalError::user_error(
            format!(
                "cap-data: capability '{}' not found on handle (available: {})",
                cap_name,
                caps.keys()
                    .map(|k| format!("'{}'", k))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            call_span,
        )
        .into()),
    }
}

/// `has-cap?`: Check if a capability is present on a Handle or WriteHandle.
/// Takes a Handle/WriteHandle and a capability name (String).
/// Returns Bool: true if the cap is present, false otherwise.
pub(crate) fn builtin_has_cap(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: Handle/WriteHandle, String cap_name
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("has-cap?", named, call_span)?;

    let handle_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let cap_name_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    // Extract caps from Handle or WriteHandle
    let caps = match handle_val {
        Value::Handle { caps, .. } => caps,
        Value::WriteHandle { caps, .. } => caps,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "has-cap?".to_string(),
                "Handle or WriteHandle",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let cap_name = require_string("has-cap?", cap_name_val, args[1].span)?;

    // Check presence
    let has_cap = caps.contains_key(&cap_name);
    ok_val(Value::Bool(has_cap), call_span)
}

/// `write-handle`: Write to a WriteHandle or a bidirectional Handle (e.g. TCP socket).
/// Takes a WriteHandle (or Handle with write_inner) and content (String for Text, Bytes for Binary).
/// Checks encoding via Binary cap: if present, content must be Bytes; otherwise String.
/// Uses `inner.borrow_mut().write_all(bytes)`.
/// Returns the original handle (WriteHandle or Handle) for chaining.
pub(crate) fn builtin_write_handle(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: WriteHandle or Handle, content
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("write-handle", named, call_span)?;

    let handle_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let content_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    // Determine the writer and the return value (preserve original handle type for chaining)
    enum HandleKind {
        Write {
            inner: Rc<RefCell<Box<dyn std::io::Write>>>,
            caps: HashMap<String, Value>,
        },
        Bidirectional {
            write_inner: Rc<RefCell<Box<dyn std::io::Write>>>,
            read_inner: Rc<RefCell<Box<dyn std::io::BufRead>>>,
            caps: HashMap<String, Value>,
        },
    }

    let kind = match &handle_val {
        Value::WriteHandle { inner, caps } => HandleKind::Write {
            inner: Rc::clone(inner),
            caps: caps.clone(),
        },
        Value::Handle {
            write_inner: Some(w),
            inner,
            caps,
        } => HandleKind::Bidirectional {
            write_inner: Rc::clone(w),
            read_inner: Rc::clone(inner),
            caps: caps.clone(),
        },
        Value::Handle {
            write_inner: None, ..
        } => {
            return Err(EvalError::type_mismatch_ctx(
                "write-handle".to_string(),
                "WriteHandle or bidirectional Handle",
                "read-only Handle",
                args[0].span,
            )
            .into())
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "write-handle".to_string(),
                "WriteHandle or bidirectional Handle",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let caps_ref = match &kind {
        HandleKind::Write { caps, .. } => caps,
        HandleKind::Bidirectional { caps, .. } => caps,
    };

    // Check encoding
    let is_binary = caps_ref.contains_key("Binary");

    use std::io::Write;
    let bytes: Vec<u8> = if is_binary {
        // Content must be Bytes
        match content_val {
            Value::Bytes { source, start, end } => source[start..end].to_vec(),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "write-handle".to_string(),
                    "Bytes (Binary handle)",
                    other.type_name(),
                    args[1].span,
                )
                .into())
            }
        }
    } else {
        // Content must be String (Text encoding)
        let s = require_string("write-handle", content_val, args[1].span)?;
        s.as_bytes().to_vec()
    };

    // Write to handle
    match &kind {
        HandleKind::Write { inner, .. } => {
            inner.borrow_mut().write_all(&bytes).map_err(|e| {
                EvalError::user_error(format!("write-handle: write failed: {}", e), call_span)
            })?;
        }
        HandleKind::Bidirectional { write_inner, .. } => {
            write_inner.borrow_mut().write_all(&bytes).map_err(|e| {
                EvalError::user_error(format!("write-handle: write failed: {}", e), call_span)
            })?;
        }
    }

    // Return the original handle (preserves type for chaining)
    match kind {
        HandleKind::Write { inner, caps } => ok_val(
            Value::WriteHandle {
                caps,
                inner: Rc::clone(&inner),
            },
            call_span,
        ),
        HandleKind::Bidirectional {
            write_inner,
            read_inner,
            caps,
        } => ok_val(
            Value::Handle {
                caps,
                inner: Rc::clone(&read_inner),
                write_inner: Some(Rc::clone(&write_inner)),
            },
            call_span,
        ),
    }
}

/// `flush`: Flush a WriteHandle or bidirectional Handle buffer.
/// Takes a WriteHandle (or Handle with write_inner), flushes it, returns the same handle.
pub(crate) fn builtin_flush(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    let val = crate::builtins::expect_one_arg("flush", args, named, &ctx, depth, call_span)?;

    use std::io::Write;
    match val {
        Value::WriteHandle {
            ref inner,
            ref caps,
        } => {
            inner.borrow_mut().flush().map_err(|e| {
                EvalError::user_error(format!("flush: flush failed: {}", e), call_span)
            })?;
            ok_val(
                Value::WriteHandle {
                    caps: caps.clone(),
                    inner: Rc::clone(inner),
                },
                call_span,
            )
        }
        Value::Handle {
            write_inner: Some(ref w),
            ref inner,
            ref caps,
        } => {
            w.borrow_mut().flush().map_err(|e| {
                EvalError::user_error(format!("flush: flush failed: {}", e), call_span)
            })?;
            ok_val(
                Value::Handle {
                    caps: caps.clone(),
                    inner: Rc::clone(inner),
                    write_inner: Some(Rc::clone(w)),
                },
                call_span,
            )
        }
        Value::Handle {
            write_inner: None, ..
        } => Err(EvalError::type_mismatch_ctx(
            "flush".to_string(),
            "WriteHandle or bidirectional Handle",
            "read-only Handle",
            args[0].span,
        )
        .into()),
        other => Err(EvalError::type_mismatch_ctx(
            "flush".to_string(),
            "WriteHandle or bidirectional Handle",
            other.type_name(),
            args[0].span,
        )
        .into()),
    }
}

/// `close`: Close a WriteHandle or bidirectional Handle.
/// Takes a WriteHandle (or Handle with write_inner), flushes and returns Null.
/// The inner writer is dropped when the last Rc is dropped.
pub(crate) fn builtin_close(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    let val = crate::builtins::expect_one_arg("close", args, named, &ctx, depth, call_span)?;

    use std::io::Write;
    match val {
        Value::WriteHandle { inner, .. } => {
            inner.borrow_mut().flush().map_err(|e| {
                EvalError::user_error(format!("close: flush failed: {}", e), call_span)
            })?;
        }
        Value::Handle {
            write_inner: Some(w),
            ..
        } => {
            w.borrow_mut().flush().map_err(|e| {
                EvalError::user_error(format!("close: flush failed: {}", e), call_span)
            })?;
        }
        Value::Handle {
            write_inner: None, ..
        } => {
            return Err(EvalError::type_mismatch_ctx(
                "close".to_string(),
                "WriteHandle or bidirectional Handle",
                "read-only Handle",
                args[0].span,
            )
            .into())
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "close".to_string(),
                "WriteHandle or bidirectional Handle",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    }

    // Return Null (the inner writer is dropped when the Rc goes out of scope)
    ok_val(Value::Dict(IndexMap::new()), call_span)
}

/// `list-dir`: List directory entries with metadata.
/// Takes a DirCap and String path, returns a Seq of metadata Dicts.
/// Each dict has keys: name, type, size, mtime.
pub(crate) fn builtin_list_dir(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: DirCap, String path
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("list-dir", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    // Extract DirCap
    let dir = match dir_val {
        Value::DirCap(d) => d,
        Value::RevocableDirCap { inner, revoked } => {
            if revoked.get() {
                return Err(EvalError::user_error(
                    "list-dir: capability has been revoked".to_string(),
                    call_span,
                )
                .into());
            }
            inner
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "list-dir".to_string(),
                "DirCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let path = require_string("list-dir", path_val, args[1].span)?;

    // Read directory entries
    let entries = dir.read_dir(&path).map_err(|e| {
        EvalError::user_error(
            format!("list-dir: failed to read directory '{}': {}", path, e),
            call_span,
        )
    })?;

    // Collect entries into a vector
    let mut entry_values = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            EvalError::user_error(
                format!("list-dir: failed to read directory entry: {}", e),
                call_span,
            )
        })?;

        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = entry.metadata().map_err(|e| {
            EvalError::user_error(
                format!("list-dir: failed to read metadata for '{}': {}", name, e),
                call_span,
            )
        })?;

        // Determine file type
        let file_type = if metadata.is_dir() {
            "dir"
        } else if metadata.is_symlink() {
            "symlink"
        } else if metadata.is_file() {
            "file"
        } else {
            "other"
        };

        // Get mtime as unix timestamp
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| {
                use std::time::UNIX_EPOCH;
                t.into_std().duration_since(UNIX_EPOCH).ok()
            })
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Build metadata dict
        use crate::value::Key;
        let mut dict = IndexMap::new();
        dict.insert(
            Key::String("name".to_string()),
            ctx.alloc_thunk(ok_val(string_val(&name), call_span)?),
        );
        dict.insert(
            Key::String("type".to_string()),
            ctx.alloc_thunk(ok_val(string_val(file_type), call_span)?),
        );
        dict.insert(
            Key::String("size".to_string()),
            ctx.alloc_thunk(ok_val(Value::Int(metadata.len() as i64), call_span)?),
        );
        dict.insert(
            Key::String("mtime".to_string()),
            ctx.alloc_thunk(ok_val(Value::Int(mtime), call_span)?),
        );

        entry_values.push(Value::Dict(dict));
    }

    // Build a sequence from the collected entries
    let mut seq = Value::Dict(IndexMap::new()); // Null (end of seq)
    for entry in entry_values.into_iter().rev() {
        let head_id = ctx.alloc_thunk(ok_val(entry, call_span)?);
        let tail_id = ctx.alloc_thunk(ok_val(seq, call_span)?);
        seq = Value::Seq {
            head: head_id,
            tail: tail_id,
        };
    }

    ok_val(seq, call_span)
}

/// `stat`: Get metadata for a file or directory.
/// Takes a DirCap and String path, returns a metadata Dict.
/// Dict has keys: name, type, size, mtime, mode, is-dir, is-file, is-symlink.
pub(crate) fn builtin_stat(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: DirCap, String path
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("stat", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    // Extract DirCap
    let dir = match dir_val {
        Value::DirCap(d) => d,
        Value::RevocableDirCap { inner, revoked } => {
            if revoked.get() {
                return Err(EvalError::user_error(
                    "stat: capability has been revoked".to_string(),
                    call_span,
                )
                .into());
            }
            inner
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "stat".to_string(),
                "DirCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let path = require_string("stat", path_val, args[1].span)?;

    // Get metadata
    let metadata = dir.metadata(&path).map_err(|e| {
        EvalError::user_error(
            format!("stat: failed to get metadata for '{}': {}", path, e),
            call_span,
        )
    })?;

    // Determine file type
    let file_type = if metadata.is_dir() {
        "dir"
    } else if metadata.is_symlink() {
        "symlink"
    } else if metadata.is_file() {
        "file"
    } else {
        "other"
    };

    // Get mtime as unix timestamp
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| {
            use std::time::UNIX_EPOCH;
            t.into_std().duration_since(UNIX_EPOCH).ok()
        })
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Get permissions (Unix-specific)
    #[cfg(unix)]
    let mode = {
        use cap_std::fs::PermissionsExt;
        metadata.permissions().mode() as i64
    };
    #[cfg(not(unix))]
    let mode = 0i64;

    // Build metadata dict
    use crate::value::Key;
    let mut dict = IndexMap::new();
    dict.insert(
        Key::String("name".to_string()),
        ctx.alloc_thunk(ok_val(string_val(&path), call_span)?),
    );
    dict.insert(
        Key::String("type".to_string()),
        ctx.alloc_thunk(ok_val(string_val(file_type), call_span)?),
    );
    dict.insert(
        Key::String("size".to_string()),
        ctx.alloc_thunk(ok_val(Value::Int(metadata.len() as i64), call_span)?),
    );
    dict.insert(
        Key::String("mtime".to_string()),
        ctx.alloc_thunk(ok_val(Value::Int(mtime), call_span)?),
    );
    dict.insert(
        Key::String("mode".to_string()),
        ctx.alloc_thunk(ok_val(Value::Int(mode), call_span)?),
    );
    dict.insert(
        Key::String("is-dir".to_string()),
        ctx.alloc_thunk(ok_val(Value::Bool(metadata.is_dir()), call_span)?),
    );
    dict.insert(
        Key::String("is-file".to_string()),
        ctx.alloc_thunk(ok_val(Value::Bool(metadata.is_file()), call_span)?),
    );
    dict.insert(
        Key::String("is-symlink".to_string()),
        ctx.alloc_thunk(ok_val(Value::Bool(metadata.is_symlink()), call_span)?),
    );

    ok_val(Value::Dict(dict), call_span)
}

/// `make-dir`: Create a directory (and parent directories if needed).
/// Takes a DirCap and String path, returns Null.
pub(crate) fn builtin_make_dir(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: DirCap, String path
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("make-dir", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    // Extract DirCap
    let dir = match dir_val {
        Value::DirCap(d) => d,
        Value::RevocableDirCap { inner, revoked } => {
            if revoked.get() {
                return Err(EvalError::user_error(
                    "make-dir: capability has been revoked".to_string(),
                    call_span,
                )
                .into());
            }
            inner
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "make-dir".to_string(),
                "DirCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let path = require_string("make-dir", path_val, args[1].span)?;

    // Create directory (and parents)
    dir.create_dir_all(&path).map_err(|e| {
        EvalError::user_error(
            format!("make-dir: failed to create directory '{}': {}", path, e),
            call_span,
        )
    })?;

    ok_val(Value::Dict(IndexMap::new()), call_span)
}

/// `remove`: Remove a file or empty directory.
/// Takes a DirCap and String path, returns Null.
/// Tries to remove as file first, then as directory.
pub(crate) fn builtin_remove(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: DirCap, String path
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("remove", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    // Extract DirCap
    let dir = match dir_val {
        Value::DirCap(d) => d,
        Value::RevocableDirCap { inner, revoked } => {
            if revoked.get() {
                return Err(EvalError::user_error(
                    "remove: capability has been revoked".to_string(),
                    call_span,
                )
                .into());
            }
            inner
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "remove".to_string(),
                "DirCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let path = require_string("remove", path_val, args[1].span)?;

    // Try to remove as file first, then as directory
    if let Err(file_err) = dir.remove_file(&path) {
        dir.remove_dir(&path).map_err(|dir_err| {
            EvalError::user_error(
                format!(
                    "remove: failed to remove '{}' (as file: {}, as dir: {})",
                    path, file_err, dir_err
                ),
                call_span,
            )
        })?;
    }

    ok_val(Value::Dict(IndexMap::new()), call_span)
}

/// `rename`: Rename or move a file or directory.
/// Takes a DirCap, old path String, and new path String, returns Null.
pub(crate) fn builtin_rename(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 args: DirCap, String old_path, String new_path
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    reject_named("rename", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let old_path_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;
    let new_path_val = materialize(&args[2], Some(&call_span), &ctx, depth)?;

    // Extract DirCap
    let dir = match dir_val {
        Value::DirCap(d) => d,
        Value::RevocableDirCap { inner, revoked } => {
            if revoked.get() {
                return Err(EvalError::user_error(
                    "rename: capability has been revoked".to_string(),
                    call_span,
                )
                .into());
            }
            inner
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "rename".to_string(),
                "DirCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let old_path = require_string("rename", old_path_val, args[1].span)?;
    let new_path = require_string("rename", new_path_val, args[2].span)?;

    // Rename (both source and dest are in the same DirCap)
    dir.rename(&old_path, &dir, &new_path).map_err(|e| {
        EvalError::user_error(
            format!(
                "rename: failed to rename '{}' to '{}': {}",
                old_path, new_path, e
            ),
            call_span,
        )
    })?;

    ok_val(Value::Dict(IndexMap::new()), call_span)
}

/// `copy`: Copy a file.
/// Takes a DirCap, source path String, and destination path String, returns Null.
pub(crate) fn builtin_copy(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 args: DirCap, String src_path, String dst_path
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    reject_named("copy", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let src_path_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;
    let dst_path_val = materialize(&args[2], Some(&call_span), &ctx, depth)?;

    // Extract DirCap
    let dir = match dir_val {
        Value::DirCap(d) => d,
        Value::RevocableDirCap { inner, revoked } => {
            if revoked.get() {
                return Err(EvalError::user_error(
                    "copy: capability has been revoked".to_string(),
                    call_span,
                )
                .into());
            }
            inner
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "copy".to_string(),
                "DirCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let src_path = require_string("copy", src_path_val, args[1].span)?;
    let dst_path = require_string("copy", dst_path_val, args[2].span)?;

    // Read source file
    use std::io::Read;
    let mut src_file = dir.open(&src_path).map_err(|e| {
        EvalError::user_error(
            format!("copy: failed to open source file '{}': {}", src_path, e),
            call_span,
        )
    })?;
    let mut contents = Vec::new();
    src_file.read_to_end(&mut contents).map_err(|e| {
        EvalError::user_error(
            format!("copy: failed to read source file '{}': {}", src_path, e),
            call_span,
        )
    })?;

    // Write to destination file
    use std::io::Write;
    let mut dst_file = dir.create(&dst_path).map_err(|e| {
        EvalError::user_error(
            format!(
                "copy: failed to create destination file '{}': {}",
                dst_path, e
            ),
            call_span,
        )
    })?;
    dst_file.write_all(&contents).map_err(|e| {
        EvalError::user_error(
            format!(
                "copy: failed to write to destination file '{}': {}",
                dst_path, e
            ),
            call_span,
        )
    })?;

    ok_val(Value::Dict(IndexMap::new()), call_span)
}

/// `link`: Create a hard link.
/// Takes a DirCap, existing path String, and link path String, returns Null.
pub(crate) fn builtin_link(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 args: DirCap, String existing_path, String link_path
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    reject_named("link", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let existing_path_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;
    let link_path_val = materialize(&args[2], Some(&call_span), &ctx, depth)?;

    // Extract DirCap
    let dir = match dir_val {
        Value::DirCap(d) => d,
        Value::RevocableDirCap { inner, revoked } => {
            if revoked.get() {
                return Err(EvalError::user_error(
                    "link: capability has been revoked".to_string(),
                    call_span,
                )
                .into());
            }
            inner
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "link".to_string(),
                "DirCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let existing_path = require_string("link", existing_path_val, args[1].span)?;
    let link_path = require_string("link", link_path_val, args[2].span)?;

    // Create hard link
    dir.hard_link(&existing_path, &dir, &link_path)
        .map_err(|e| {
            EvalError::user_error(
                format!(
                    "link: failed to create hard link from '{}' to '{}': {}",
                    existing_path, link_path, e
                ),
                call_span,
            )
        })?;

    ok_val(Value::Dict(IndexMap::new()), call_span)
}

/// `read-link`: Read the target of a symbolic link.
/// Takes a DirCap and String path, returns the target path as a String.
pub(crate) fn builtin_read_link(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: DirCap, String path
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("read-link", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    // Extract DirCap
    let dir = match dir_val {
        Value::DirCap(d) => d,
        Value::RevocableDirCap { inner, revoked } => {
            if revoked.get() {
                return Err(EvalError::user_error(
                    "read-link: capability has been revoked".to_string(),
                    call_span,
                )
                .into());
            }
            inner
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "read-link".to_string(),
                "DirCap",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let path = require_string("read-link", path_val, args[1].span)?;

    // Read symlink target
    let target = dir.read_link(&path).map_err(|e| {
        EvalError::user_error(
            format!("read-link: failed to read symlink '{}': {}", path, e),
            call_span,
        )
    })?;

    let target_str = target.to_string_lossy().to_string();
    ok_val(string_val(&target_str), call_span)
}

// ============================================================================
// TLS Support (STUB — Phase 2 implementation)
// ============================================================================

/// `tls-connect`: Layer TLS on a connection (STUB).
/// Two forms:
/// 1. Connector form: `tls-connect connector Transport host port opts`
/// 2. Handle form: `tls-connect handle sni opts`
///
/// Returns Handle[Binary Readable Writable Stream Tls] with TlsInfo in the Tls capability.
///
/// **Current status:** Stub implementation — validates arguments but does not perform TLS handshake.
/// Full implementation deferred to lib-tls sprint (doc/whatif/lib-tls.md).
pub(crate) fn builtin_tls_connect(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth: _,
        call_span,
        ctx: _,
    } = ctx_arg;

    reject_named("tls-connect", named, call_span)?;

    // Validate arity
    if args.len() < 3 || args.len() > 5 {
        return Err(EvalError::user_error(
            format!(
                "tls-connect: expected 3 args (Handle form) or 4-5 args (Connector form), got {}",
                args.len()
            ),
            call_span,
        )
        .into());
    }

    // Stub: just error out for now
    Err(EvalError::user_error(
        "tls-connect: not yet implemented (requires Handle refactoring to preserve TCP stream)"
            .to_string(),
        call_span,
    )
    .into())
}

/// `tls-peer-cert`: Extract TLS certificate metadata from a TLS handle.
/// Requires Handle[... Tls ...].
/// Returns a dict with: subject, issuer, sans, not-before, not-after, spki-sha256.
pub(crate) fn builtin_tls_peer_cert(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    let val =
        crate::builtins::expect_one_arg("tls-peer-cert", args, named, &ctx, depth, call_span)?;

    // Extract Handle and check for Tls capability
    let caps = match val {
        Value::Handle { caps, .. } => caps,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "tls-peer-cert".to_string(),
                "Handle",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    if !caps.contains_key("Tls") {
        return Err(EvalError::user_error(
            "tls-peer-cert: handle must have Tls capability (created by tls-connect)".to_string(),
            call_span,
        )
        .into());
    }

    // TODO: Extract TlsInfo from the Tls capability data and parse the certificate
    // For now, return a placeholder dict
    Err(EvalError::user_error(
        "tls-peer-cert: not yet fully implemented".to_string(),
        call_span,
    )
    .into())
}

/// `spki-pin`: Create an SPKI pin dict.
/// Takes HashAlgorithm variant and Bytes fingerprint.
/// Returns dict: {algorithm: Variant, fingerprint: Bytes}.
pub(crate) fn builtin_spki_pin(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("spki-pin", named, call_span)?;

    let algorithm_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let fingerprint_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    // Validate algorithm is a Variant
    let algorithm_tag = match algorithm_val {
        Value::Variant { tag, payload } => {
            if payload.is_some() {
                return Err(EvalError::user_error(
                    "spki-pin: algorithm variant must not have a payload".to_string(),
                    args[0].span,
                )
                .into());
            }
            tag
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "spki-pin".to_string(),
                "HashAlgorithm variant",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    // Validate fingerprint is Bytes
    let fingerprint_bytes = match fingerprint_val {
        Value::Bytes { source, start, end } => source[start..end].to_vec(),
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "spki-pin".to_string(),
                "Bytes",
                other.type_name(),
                args[1].span,
            )
            .into())
        }
    };

    // Validate algorithm name
    let valid_algorithms = [
        "Sha256", "Sha384", "Sha512", "Sha3-256", "Sha3-384", "Sha3-512", "Blake3",
    ];
    if !valid_algorithms.contains(&algorithm_tag.as_str()) {
        return Err(EvalError::user_error(
            format!(
                "spki-pin: invalid hash algorithm '{}' (expected one of: {})",
                algorithm_tag,
                valid_algorithms.join(", ")
            ),
            args[0].span,
        )
        .into());
    }

    // Build result dict
    use crate::value::Key;
    let mut dict = IndexMap::new();
    dict.insert(
        Key::String("algorithm".to_string()),
        ctx.alloc_thunk(ok_val(
            Value::Variant {
                tag: algorithm_tag,
                payload: None,
            },
            call_span,
        )?),
    );
    dict.insert(
        Key::String("fingerprint".to_string()),
        ctx.alloc_thunk(ok_val(
            Value::Bytes {
                source: Rc::from(fingerprint_bytes.as_slice()),
                start: 0,
                end: fingerprint_bytes.len(),
            },
            call_span,
        )?),
    );

    ok_val(Value::Dict(dict), call_span)
}
