//! Filesystem and network I/O builtins: open, slurp, write, write-atomic, connect, lines.
//!
//! These builtins provide capability-based access to filesystems and networks,
//! implementing object-capability security through DirCap and NetCap values.
//!
//! **Filesystem builtins:**
//! - `open`: Open a file within a DirCap
//! - `slurp`: Read all bytes from a Handle (returns Str for Text, Bytes for Binary)
//! - `write`: Write a string to a file (DirCap-based)
//! - `write-atomic`: Atomically write to a file (temp + rename)
//! - `narrow`: Attenuate a DirCap to a subdirectory
//! - `revocable`: Wrap a DirCap in a revocable wrapper
//! - `revoke-cap`: Revoke a RevocableDirCap
//!
//! **Network builtins:**
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
use std::io::BufReader;
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
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("emit", args, named, &ctx, call_span)?;
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
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("env", args, named, &ctx, call_span)?;
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

/// `open`: Open a file within a DirCap.
///
/// Accepts two calling patterns:
/// 1. Legacy (3 args): `[open dir_cap path "r"]` — backward compatibility with string mode
/// 2. Variant flags (3+ args): `[open dir_cap path Readable Text]` — each arg after path
///    is a Variant flag that sets a capability in the returned Handle's caps HashMap.
///
/// Variant flags:
/// - `Readable` → read mode (mutually exclusive with Writable)
/// - `Writable` → write mode (mutually exclusive with Readable)
/// - `Binary` → binary encoding (mutually exclusive with Text)
/// - `Text` → text encoding (default if neither Binary nor Text specified)
///
/// Returns Value::Handle (read mode) or Value::WriteHandle (write mode).
///
/// At least one flag is required in the new pattern. If neither Readable nor Writable is
/// specified, an error is returned.
pub(crate) fn builtin_open(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    // Require at least 3 args: DirCap, String path, and at least one flag/mode
    if args.len() < 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    reject_named("open", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx)?;

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

    // Check if third arg is a String (legacy mode) or Variant (new mode)
    let third_arg_val = materialize(&args[2], Some(&call_span), &ctx)?;

    // Legacy string mode check
    if matches!(third_arg_val, Value::String { .. }) {
        // BACKWARD COMPATIBILITY PATH: 3-arg string mode
        if args.len() != 3 {
            return Err(EvalError::user_error(
                "open: string mode requires exactly 3 arguments (dir, path, mode)".to_string(),
                call_span,
            )
            .into());
        }

        let mode = require_string("open", third_arg_val, args[2].span)?;

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

        // Default caps for read-only handle (legacy mode)
        let mut caps = HashMap::new();
        caps.insert("Readable".to_string(), Value::Dict(IndexMap::new())); // Null
        caps.insert("Text".to_string(), Value::Dict(IndexMap::new())); // Null

        return ok_val(
            Value::Handle {
                caps,
                inner: Rc::new(std::cell::RefCell::new(handle)),
                write_inner: None,
                seek_inner: None,
                raw_tcp: None,
                creation_span: call_span,
            },
            call_span,
        );
    }

    // NEW VARIANT FLAGS PATH: parse flags from args[2..]
    let mut caps = HashMap::new();
    let mut has_readable = false;
    let mut has_writable = false;
    let mut has_binary = false;
    let mut has_text = false;
    let mut has_seekable = false;

    for flag_arg in &args[2..] {
        let flag_val = materialize(flag_arg, Some(&call_span), &ctx)?;

        match flag_val {
            Value::Variant { ref tag, .. } => match tag.as_str() {
                "Readable" => {
                    if has_writable {
                        return Err(EvalError::user_error(
                            "open: cannot specify both Readable and Writable flags".to_string(),
                            call_span,
                        )
                        .into());
                    }
                    has_readable = true;
                    caps.insert("Readable".to_string(), Value::Dict(IndexMap::new()));
                }
                "Writable" => {
                    if has_readable {
                        return Err(EvalError::user_error(
                            "open: cannot specify both Readable and Writable flags".to_string(),
                            call_span,
                        )
                        .into());
                    }
                    has_writable = true;
                    caps.insert("Writable".to_string(), Value::Dict(IndexMap::new()));
                }
                "Binary" => {
                    if has_text {
                        return Err(EvalError::user_error(
                            "open: cannot specify both Binary and Text flags".to_string(),
                            call_span,
                        )
                        .into());
                    }
                    has_binary = true;
                    caps.insert("Binary".to_string(), Value::Dict(IndexMap::new()));
                }
                "Text" => {
                    if has_binary {
                        return Err(EvalError::user_error(
                            "open: cannot specify both Binary and Text flags".to_string(),
                            call_span,
                        )
                        .into());
                    }
                    has_text = true;
                    caps.insert("Text".to_string(), Value::Dict(IndexMap::new()));
                }
                "Seekable" => {
                    has_seekable = true;
                    caps.insert("Seekable".to_string(), Value::Dict(IndexMap::new()));
                }
                other => {
                    return Err(EvalError::user_error(
                            format!(
                                "open: unknown capability flag '{}' (expected Readable, Writable, Binary, Text, or Seekable)",
                                other
                            ),
                            call_span,
                        )
                        .into());
                }
            },
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "open".to_string(),
                    "Variant (capability flag)",
                    other.type_name(),
                    flag_arg.span,
                )
                .into());
            }
        }
    }

    // Require at least one of Readable or Writable
    if !has_readable && !has_writable {
        return Err(EvalError::user_error(
            "open: must specify at least one of Readable or Writable flags".to_string(),
            call_span,
        )
        .into());
    }

    // Default to Text encoding if neither Binary nor Text specified
    if !has_binary && !has_text {
        caps.insert("Text".to_string(), Value::Dict(IndexMap::new()));
    }

    // Open the file based on flags
    use std::io::BufReader;
    if has_readable {
        // Read mode
        let file = dir.open(&path).map_err(|e| {
            EvalError::user_error(
                format!("open: failed to open file '{}': {}", path, e),
                call_span,
            )
        })?;

        // If Seekable, clone the file handle for seeking operations
        // We need two handles: one wrapped in BufReader for reading, one for seeking
        let seek_inner = if has_seekable {
            let seek_file = file.try_clone().map_err(|e| {
                EvalError::user_error(
                    format!("open: failed to clone file handle for seeking: {}", e),
                    call_span,
                )
            })?;
            Some(Rc::new(std::cell::RefCell::new(
                Box::new(BufReader::new(seek_file)) as Box<dyn std::io::Seek>,
            )))
        } else {
            None
        };

        let handle: Box<dyn std::io::BufRead> = Box::new(BufReader::new(file));

        ok_val(
            Value::Handle {
                caps,
                inner: Rc::new(std::cell::RefCell::new(handle)),
                write_inner: None,
                seek_inner,
                raw_tcp: None,
                creation_span: call_span,
            },
            call_span,
        )
    } else {
        // Write mode (has_writable == true)
        return Err(EvalError::user_error(
            "open: Writable mode not yet implemented (Phase 1 is read-only)".to_string(),
            call_span,
        )
        .into());
    }
}

/// `slurp`: Read all bytes from a Handle to a String or Bytes.
/// Takes a Handle, reads to EOF, returns String (if Text encoding) or Bytes (if Binary encoding).
pub(crate) fn builtin_slurp(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("slurp", args, named, &ctx, call_span)?;

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
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: DirCap, String subpath
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("narrow", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let subpath_val = materialize(&args[1], Some(&call_span), &ctx)?;

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
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("revocable", args, named, &ctx, call_span)?;

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
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("revoke-cap", args, named, &ctx, call_span)?;

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

/// `connect`: Open a TCP or UDP connection within a NetCap.
/// Takes a NetCap, hostname String, port Int, and optional Transport variant (default: Tcp).
/// - `Tcp` (default) → Handle[Binary Readable Writable Stream]
/// - `Udp` → error "UDP not yet supported, use Tcp" (reserved for Phase 2)
pub(crate) fn builtin_connect(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    reject_named("connect", named, call_span)?;

    // Force args[1] (Transport tag) first — this is a STRICTNESS POINT
    // Minimum 2 args: cap and transport
    if args.len() < 2 {
        return Err(EvalError::user_error(
            format!(
                "connect: expected at least 2 arguments (cap transport [...address]), got {}",
                args.len()
            ),
            call_span,
        )
        .into());
    }

    let cap_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let transport_val = materialize(&args[1], Some(&call_span), &ctx)?;

    // Extract Transport variant tag
    let transport_tag = match transport_val {
        Value::Variant { tag, .. } => tag,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "connect".to_string(),
                "Transport variant (e.g., Tcp, Udp)",
                other.type_name(),
                args[1].span,
            )
            .into())
        }
    };

    // Dispatch on transport tag to determine address format and arg count
    match transport_tag.as_str() {
        "Tcp" => {
            // Tcp requires: cap Tcp host port (4 args total)
            if args.len() != 4 {
                return Err(EvalError::user_error(
                    format!(
                        "connect: Tcp transport requires host and port (4 args total), got {}",
                        args.len()
                    ),
                    call_span,
                )
                .into());
            }
            // Continue with TCP connection below
        }
        "Udp" => {
            // Udp requires: cap Udp host port (4 args total)
            if args.len() != 4 {
                return Err(EvalError::user_error(
                    format!(
                        "connect: Udp transport requires host and port (4 args total), got {}",
                        args.len()
                    ),
                    call_span,
                )
                .into());
            }
            // UDP is a datagram protocol - current Handle infrastructure doesn't support
            // datagram semantics (recv_from/send_to). Need dedicated Datagram handle type.
            return Err(EvalError::user_error(
                "connect: UDP not yet implemented — requires Datagram handle infrastructure"
                    .to_string(),
                call_span,
            )
            .into());
        }
        "UnixStream" => {
            // UnixStream requires: cap UnixStream path (3 args total)
            if args.len() != 3 {
                return Err(EvalError::user_error(
                    format!(
                        "connect: UnixStream transport requires path (3 args total), got {}",
                        args.len()
                    ),
                    call_span,
                )
                .into());
            }
            // Continue with Unix stream connection below
        }
        "UnixDatagram" => {
            // UnixDatagram requires: cap UnixDatagram path (3 args total)
            if args.len() != 3 {
                return Err(EvalError::user_error(
                    format!(
                        "connect: UnixDatagram transport requires path (3 args total), got {}",
                        args.len()
                    ),
                    call_span,
                )
                .into());
            }
            // UnixDatagram is a datagram protocol - current Handle infrastructure doesn't support
            // datagram semantics (recv_from/send_to). Need dedicated Datagram handle type.
            return Err(EvalError::user_error(
                "connect: UnixDatagram not yet implemented — requires Datagram handle infrastructure"
                    .to_string(),
                call_span,
            )
            .into());
        }
        "NamedPipe" => {
            // NamedPipe not supported on non-Windows platforms
            return Err(EvalError::user_error(
                "connect: Named pipes not supported on this platform".to_string(),
                call_span,
            )
            .into());
        }
        "Icmp" => {
            // ICMP requires raw sockets or elevated privileges — not yet implemented
            return Err(EvalError::user_error(
                "connect: ICMP not yet implemented — requires platform-specific raw socket support"
                    .to_string(),
                call_span,
            )
            .into());
        }
        other => {
            return Err(EvalError::user_error(
                format!("connect: unsupported transport '{}'", other),
                call_span,
            )
            .into());
        }
    }

    // Branch based on transport type
    match transport_tag.as_str() {
        "Tcp" => {
            // TCP path
            let host_val = materialize(&args[2], Some(&call_span), &ctx)?;
            let port_val = materialize(&args[3], Some(&call_span), &ctx)?;

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

            let host = require_string("connect", host_val, args[2].span)?;
            let port = match port_val {
                Value::Int(n) if n >= 1 && n <= 65535 => n as u16,
                Value::Int(_) => {
                    return Err(EvalError::user_error(
                        "connect: port must be 1-65535".to_string(),
                        args[3].span,
                    )
                    .into())
                }
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "connect".to_string(),
                        "Int",
                        other.type_name(),
                        args[3].span,
                    )
                    .into())
                }
            };

            // Check allowlist before connecting
            // Returns Some(ip) if we need to connect to a resolved IP (DNS rebinding mitigation)
            let resolved_ip = check_net_cap_allowlist(&entries, &host, Some(port), call_span)?;

            // Open TCP connection
            // If DNS resolution was required, connect to the resolved IP to mitigate DNS rebinding
            let addr = if let Some(ip) = resolved_ip {
                format!("{}:{}", ip, port)
            } else {
                format!("{}:{}", host, port)
            };
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

            // Clone stream for tls-layer extraction before consuming into BufReader
            let raw_tcp_stream = stream.try_clone().map_err(|e| {
                EvalError::user_error(
                    format!("connect: failed to clone TcpStream for raw_tcp: {}", e),
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
                    seek_inner: None,
                    raw_tcp: Some(Rc::new(RefCell::new(Some(raw_tcp_stream)))),
                    creation_span: call_span,
                },
                call_span,
            )
        }
        "UnixStream" => {
            // Unix stream socket path
            #[cfg(target_os = "linux")]
            {
                let path_val = materialize(&args[2], Some(&call_span), &ctx)?;

                // Extract DirCap for path validation
                let dir = match cap_val {
                    Value::DirCap(d) => d,
                    Value::RevocableDirCap { inner, revoked } => {
                        if revoked.get() {
                            return Err(EvalError::user_error(
                                "connect: capability has been revoked".to_string(),
                                call_span,
                            )
                            .into());
                        }
                        inner
                    }
                    other => {
                        return Err(EvalError::type_mismatch_ctx(
                            "connect".to_string(),
                            "DirCap",
                            other.type_name(),
                            args[0].span,
                        )
                        .into())
                    }
                };

                let path = require_string("connect", path_val, args[2].span)?;

                // Validate path doesn't try to escape the directory
                if path.contains("..") {
                    return Err(EvalError::user_error(
                        format!("connect: path cannot contain '..': {}", path),
                        call_span,
                    )
                    .into());
                }

                // Get the directory's file descriptor and resolve the full path via /proc/self/fd
                // This is necessary because Unix domain sockets need an absolute path to connect
                use std::os::unix::io::AsRawFd;
                let dir_fd = dir.as_raw_fd();
                let proc_path = std::path::PathBuf::from(format!("/proc/self/fd/{}", dir_fd));
                let dir_path = std::fs::read_link(&proc_path).map_err(|e| {
                    EvalError::user_error(
                        format!("connect: failed to resolve DirCap path: {}", e),
                        call_span,
                    )
                })?;
                let full_path = dir_path.join(&path);

                // Connect to Unix stream socket
                let stream = std::os::unix::net::UnixStream::connect(&full_path).map_err(|e| {
                    EvalError::user_error(
                        format!(
                            "connect: failed to connect to Unix socket '{}': {}",
                            path, e
                        ),
                        call_span,
                    )
                })?;

                // Clone stream for write half
                let write_stream = stream.try_clone().map_err(|e| {
                    EvalError::user_error(
                        format!("connect: failed to clone UnixStream for write half: {}", e),
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

                // Caps for Unix stream: Binary Readable Writable Stream
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
                        seek_inner: None,
                        raw_tcp: None, // Not TCP
                        creation_span: call_span,
                    },
                    call_span,
                )
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(EvalError::user_error(
                    "connect: Unix sockets not yet supported on this platform (requires Linux /proc/self/fd access)".to_string(),
                    call_span,
                )
                .into())
            }
        }
        _ => {
            // Udp, UnixDatagram, NamedPipe, and Icmp already handled in first match
            // This is unreachable — all transport types have been handled above
            unreachable!(
                "connect: transport '{}' should have been handled in first match",
                transport_tag
            )
        }
    }
}

/// Check if a connection to host:port is allowed by the NetCap allowlist.
/// Returns Ok(None) for hostname-only match, Ok(Some(ip)) for IP-based match requiring DNS resolution.
/// For host-only transports (ICMP), pass port=None — HostPort entries won't match, but Hostname/Glob/CIDR will.
fn check_net_cap_allowlist(
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
            NetCapEntry::Hostname(allowed_host) => {
                if host.eq_ignore_ascii_case(allowed_host) {
                    hostname_match = true;
                    break;
                }
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

/// `lines`: Read lines from a Handle lazily.
/// Takes a Handle, returns a lazy Seq where each element is a line (without newline).
/// This is a coinductive lazy sequence — each tail force reads the next line.
/// Errors if the Handle has a Binary cap (lines requires Text encoding).
pub(crate) fn builtin_lines(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("lines", args, named, &ctx, call_span)?;

    // Extract Handle
    let (handle, write_inner, caps) = match val {
        Value::Handle {
            inner,
            write_inner,
            caps,
            ..
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
    builtin_lines_step(handle, write_inner, caps, call_span, ctx)
}

/// Helper for `lines`: reads one line and returns Seq or null.
pub(crate) fn builtin_lines_step(
    handle: Rc<RefCell<Box<dyn std::io::BufRead>>>,
    write_inner: Option<Rc<RefCell<Box<dyn std::io::Write>>>>,
    caps: HashMap<String, Value>,
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
                    seek_inner: None,
                    raw_tcp: None,
                    creation_span: call_span,
                },
                call_span,
            )?];
            let tail = Rc::new(Thunk::new_pending_builtin(
                builtin!("lines", builtin_lines),
                tail_args,
                None,
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
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 args: DirCap, String path, String content
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    reject_named("write", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx)?;
    let content_val = materialize(&args[2], Some(&call_span), &ctx)?;

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
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 args: DirCap, String path, String content
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    reject_named("write-atomic", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx)?;
    let content_val = materialize(&args[2], Some(&call_span), &ctx)?;

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
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: Handle/WriteHandle, String cap_name
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("cap-data", named, call_span)?;

    let handle_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let cap_name_val = materialize(&args[1], Some(&call_span), &ctx)?;

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
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: Handle/WriteHandle, String cap_name
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("has-cap?", named, call_span)?;

    let handle_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let cap_name_val = materialize(&args[1], Some(&call_span), &ctx)?;

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
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: WriteHandle or Handle, content
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("write-handle", named, call_span)?;

    let handle_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let content_val = materialize(&args[1], Some(&call_span), &ctx)?;

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
            ..
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
                seek_inner: None,
                raw_tcp: None,
                creation_span: call_span,
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
        call_span,
        ctx,
    } = ctx_arg;

    let val = crate::builtins::expect_one_arg("flush", args, named, &ctx, call_span)?;

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
            ..
        } => {
            w.borrow_mut().flush().map_err(|e| {
                EvalError::user_error(format!("flush: flush failed: {}", e), call_span)
            })?;
            ok_val(
                Value::Handle {
                    caps: caps.clone(),
                    inner: Rc::clone(inner),
                    write_inner: Some(Rc::clone(w)),
                    seek_inner: None,
                    raw_tcp: None,
                    creation_span: call_span,
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
        call_span,
        ctx,
    } = ctx_arg;

    let val = crate::builtins::expect_one_arg("close", args, named, &ctx, call_span)?;

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

/// `seek`: Seek to a byte offset from the start of the file.
/// Takes a Handle and an Int offset, returns the Handle for chaining.
/// Requires the Seekable capability.
pub(crate) fn builtin_seek(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("seek", named, call_span)?;

    let handle_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let offset_val = materialize(&args[1], Some(&call_span), &ctx)?;

    // Extract offset as Int
    let offset = match offset_val {
        Value::Int(i) => i,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "seek".to_string(),
                "Int",
                other.type_name(),
                args[1].span,
            )
            .into())
        }
    };

    // Extract Handle and check for Seekable capability
    match handle_val {
        Value::Handle {
            ref caps,
            ref inner,
            ref write_inner,
            ref seek_inner,
            ..
        } => {
            // Check for Seekable capability
            if !caps.contains_key("Seekable") {
                return Err(EvalError::user_error(
                    "seek: Handle does not have Seekable capability".to_string(),
                    args[0].span,
                )
                .into());
            }

            // Get the seek_inner
            let seek_handle = match seek_inner {
                Some(s) => s,
                None => {
                    return Err(EvalError::user_error(
                        "seek: Handle has Seekable capability but no seek interface".to_string(),
                        args[0].span,
                    )
                    .into())
                }
            };

            // Perform the seek on both the inner BufReader and seek_inner
            // They are cloned File handles, so we need to seek both to keep them in sync
            use std::io::Seek;

            // Seek the seek_inner first
            seek_handle
                .borrow_mut()
                .seek(std::io::SeekFrom::Start(offset as u64))
                .map_err(|e| {
                    EvalError::user_error(format!("seek: seek failed: {}", e), call_span)
                })?;

            // Now seek the inner BufReader by downcasting
            // Since both are BufReader<cap_std::fs::File>, we can use std::any::Any
            use std::any::Any;
            let mut inner_borrow = inner.borrow_mut();
            if let Some(buf_reader) =
                (&mut *inner_borrow as &mut dyn Any).downcast_mut::<BufReader<cap_std::fs::File>>()
            {
                buf_reader
                    .seek(std::io::SeekFrom::Start(offset as u64))
                    .map_err(|e| {
                        EvalError::user_error(
                            format!("seek: inner buffer seek failed: {}", e),
                            call_span,
                        )
                    })?;
            } else {
                return Err(EvalError::user_error(
                    "seek: failed to downcast BufRead to BufReader<File>".to_string(),
                    call_span,
                )
                .into());
            }
            drop(inner_borrow); // Release the borrow before cloning

            // Return the handle for chaining
            ok_val(
                Value::Handle {
                    caps: caps.clone(),
                    inner: Rc::clone(inner),
                    write_inner: write_inner.clone(),
                    seek_inner: Some(Rc::clone(seek_handle)),
                    raw_tcp: None,
                    creation_span: call_span,
                },
                call_span,
            )
        }
        other => Err(EvalError::type_mismatch_ctx(
            "seek".to_string(),
            "Handle",
            other.type_name(),
            args[0].span,
        )
        .into()),
    }
}

/// `seek-end`: Seek to the end of the file.
/// Takes a Handle, returns the Handle for chaining.
/// Requires the Seekable capability.
pub(crate) fn builtin_seek_end(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    let val = crate::builtins::expect_one_arg("seek-end", args, named, &ctx, call_span)?;

    // Extract Handle and check for Seekable capability
    match val {
        Value::Handle {
            ref caps,
            ref inner,
            ref write_inner,
            ref seek_inner,
            ..
        } => {
            // Check for Seekable capability
            if !caps.contains_key("Seekable") {
                return Err(EvalError::user_error(
                    "seek-end: Handle does not have Seekable capability".to_string(),
                    args[0].span,
                )
                .into());
            }

            // Get the seek_inner
            let seek_handle = match seek_inner {
                Some(s) => s,
                None => {
                    return Err(EvalError::user_error(
                        "seek-end: Handle has Seekable capability but no seek interface"
                            .to_string(),
                        args[0].span,
                    )
                    .into())
                }
            };

            // Perform the seek on both the inner BufReader and seek_inner
            use std::io::Seek;

            // Seek the seek_inner first
            seek_handle
                .borrow_mut()
                .seek(std::io::SeekFrom::End(0))
                .map_err(|e| {
                    EvalError::user_error(format!("seek-end: seek failed: {}", e), call_span)
                })?;

            // Now seek the inner BufReader by downcasting
            use std::any::Any;
            let mut inner_borrow = inner.borrow_mut();
            if let Some(buf_reader) =
                (&mut *inner_borrow as &mut dyn Any).downcast_mut::<BufReader<cap_std::fs::File>>()
            {
                buf_reader.seek(std::io::SeekFrom::End(0)).map_err(|e| {
                    EvalError::user_error(
                        format!("seek-end: inner buffer seek failed: {}", e),
                        call_span,
                    )
                })?;
            } else {
                return Err(EvalError::user_error(
                    "seek-end: failed to downcast BufRead to BufReader<File>".to_string(),
                    call_span,
                )
                .into());
            }
            drop(inner_borrow); // Release the borrow before cloning

            // Return the handle for chaining
            ok_val(
                Value::Handle {
                    caps: caps.clone(),
                    inner: Rc::clone(inner),
                    write_inner: write_inner.clone(),
                    seek_inner: Some(Rc::clone(seek_handle)),
                    raw_tcp: None,
                    creation_span: call_span,
                },
                call_span,
            )
        }
        other => Err(EvalError::type_mismatch_ctx(
            "seek-end".to_string(),
            "Handle",
            other.type_name(),
            args[0].span,
        )
        .into()),
    }
}

/// `position`: Get the current byte offset in the file.
/// Takes a Handle, returns an Int.
/// Requires the Seekable capability.
pub(crate) fn builtin_position(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    let val = crate::builtins::expect_one_arg("position", args, named, &ctx, call_span)?;

    // Extract Handle and check for Seekable capability
    match val {
        Value::Handle {
            ref caps,
            ref seek_inner,
            ..
        } => {
            // Check for Seekable capability
            if !caps.contains_key("Seekable") {
                return Err(EvalError::user_error(
                    "position: Handle does not have Seekable capability".to_string(),
                    args[0].span,
                )
                .into());
            }

            // Get the seek_inner
            let seek_handle = match seek_inner {
                Some(s) => s,
                None => {
                    return Err(EvalError::user_error(
                        "position: Handle has Seekable capability but no seek interface"
                            .to_string(),
                        args[0].span,
                    )
                    .into())
                }
            };

            // Get the current position
            use std::io::Seek;
            let pos = seek_handle.borrow_mut().stream_position().map_err(|e| {
                EvalError::user_error(
                    format!("position: failed to get position: {}", e),
                    call_span,
                )
            })?;

            ok_val(Value::Int(pos as i64), call_span)
        }
        other => Err(EvalError::type_mismatch_ctx(
            "position".to_string(),
            "Handle",
            other.type_name(),
            args[0].span,
        )
        .into()),
    }
}

/// `list-dir`: List directory entries with metadata.
/// Takes a DirCap and String path, returns a Seq of metadata Dicts.
/// Each dict has keys: name, type, size, mtime.
pub(crate) fn builtin_list_dir(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: DirCap, String path
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("list-dir", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx)?;

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
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: DirCap, String path
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("stat", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx)?;

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
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: DirCap, String path
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("make-dir", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx)?;

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
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: DirCap, String path
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("remove", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx)?;

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
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 args: DirCap, String old_path, String new_path
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    reject_named("rename", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let old_path_val = materialize(&args[1], Some(&call_span), &ctx)?;
    let new_path_val = materialize(&args[2], Some(&call_span), &ctx)?;

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
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 args: DirCap, String src_path, String dst_path
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    reject_named("copy", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let src_path_val = materialize(&args[1], Some(&call_span), &ctx)?;
    let dst_path_val = materialize(&args[2], Some(&call_span), &ctx)?;

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
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 args: DirCap, String existing_path, String link_path
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    reject_named("link", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let existing_path_val = materialize(&args[1], Some(&call_span), &ctx)?;
    let link_path_val = materialize(&args[2], Some(&call_span), &ctx)?;

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
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 2 args: DirCap, String path
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("read-link", named, call_span)?;

    let dir_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx)?;

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
fn build_tls_config(
    opts_val: &Value,
    opts_span: Span,
    ctx: &Rc<crate::eval::EvalContext>,
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
        opts_dict.get(&crate::value::Key::String("no-system-roots".to_string()))
    {
        let thunk = ctx.get_thunk(*thunk_id);
        let val = materialize(&thunk, Some(&opts_span), ctx)?;
        match val {
            Value::Bool(b) => b,
            Value::Dict(ref d) if d.is_empty() => false, // Null
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "tls-connect opts.no-system-roots".to_string(),
                    "Bool",
                    other.type_name(),
                    opts_span,
                )
                .into())
            }
        }
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
                opts_span,
            )
            .into());
        }

        for cert in cert_result.certs {
            root_store.add(cert).map_err(|e| {
                EvalError::user_error(
                    format!("tls-connect: failed to add system CA cert: {}", e),
                    opts_span,
                )
            })?;
        }
    }

    // Load mozilla-roots if requested
    let mozilla_roots = if let Some(thunk_id) =
        opts_dict.get(&crate::value::Key::String("mozilla-roots".to_string()))
    {
        let thunk = ctx.get_thunk(*thunk_id);
        let val = materialize(&thunk, Some(&opts_span), ctx)?;
        match val {
            Value::Bool(b) => b,
            Value::Dict(ref d) if d.is_empty() => false, // Null
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "tls-connect opts.mozilla-roots".to_string(),
                    "Bool",
                    other.type_name(),
                    opts_span,
                )
                .into())
            }
        }
    } else {
        false
    };

    if mozilla_roots {
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }

    // Load ca-bundle if provided
    if let Some(thunk_id) = opts_dict.get(&crate::value::Key::String("ca-bundle".to_string())) {
        let thunk = ctx.get_thunk(*thunk_id);
        let handle_val = materialize(&thunk, Some(&opts_span), ctx)?;
        let pem_bytes = slurp_handle_bytes(&handle_val, opts_span)?;

        let mut cursor = std::io::Cursor::new(pem_bytes);
        let certs = rustls_pemfile::certs(&mut cursor)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                EvalError::user_error(
                    format!("tls-connect: failed to parse CA bundle PEM: {}", e),
                    opts_span,
                )
            })?;

        for cert in certs {
            root_store.add(cert).map_err(|e| {
                EvalError::user_error(
                    format!("tls-connect: failed to add CA bundle cert: {}", e),
                    opts_span,
                )
            })?;
        }
    }

    // Build config with client auth
    let has_client_cert =
        opts_dict.contains_key(&crate::value::Key::String("client-cert".to_string()));
    let has_client_key =
        opts_dict.contains_key(&crate::value::Key::String("client-key".to_string()));

    let mut config = if has_client_cert || has_client_key {
        if !has_client_cert || !has_client_key {
            return Err(EvalError::user_error(
                "tls-connect: both client-cert and client-key must be provided for mTLS"
                    .to_string(),
                opts_span,
            )
            .into());
        }

        let cert_thunk_id = opts_dict
            .get(&crate::value::Key::String("client-cert".to_string()))
            .unwrap();
        let cert_thunk = ctx.get_thunk(*cert_thunk_id);
        let cert_handle = materialize(&cert_thunk, Some(&opts_span), ctx)?;

        let key_thunk_id = opts_dict
            .get(&crate::value::Key::String("client-key".to_string()))
            .unwrap();
        let key_thunk = ctx.get_thunk(*key_thunk_id);
        let key_handle = materialize(&key_thunk, Some(&opts_span), ctx)?;

        let cert_pem = slurp_handle_bytes(&cert_handle, opts_span)?;
        let key_pem = slurp_handle_bytes(&key_handle, opts_span)?;

        let mut cert_cursor = std::io::Cursor::new(cert_pem);
        let certs = rustls_pemfile::certs(&mut cert_cursor)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                EvalError::user_error(
                    format!("tls-connect: failed to parse client cert PEM: {}", e),
                    opts_span,
                )
            })?;

        let mut key_cursor = std::io::Cursor::new(key_pem);
        let key = rustls_pemfile::private_key(&mut key_cursor)
            .map_err(|e| {
                EvalError::user_error(
                    format!("tls-connect: failed to parse client key PEM: {}", e),
                    opts_span,
                )
            })?
            .ok_or_else(|| {
                EvalError::user_error(
                    "tls-connect: no private key found in client-key PEM".to_string(),
                    opts_span,
                )
            })?;

        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_client_auth_cert(certs, key)
            .map_err(|e| {
                EvalError::user_error(
                    format!("tls-connect: failed to configure client certificate: {}", e),
                    opts_span,
                )
            })?
    } else {
        rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth()
    };

    // Set ALPN protocols
    if let Some(thunk_id) = opts_dict.get(&crate::value::Key::String("alpn".to_string())) {
        let thunk = ctx.get_thunk(*thunk_id);
        let alpn_val = materialize(&thunk, Some(&opts_span), ctx)?;
        let alpn_protocols = extract_alpn_protocols(&alpn_val, opts_span, ctx)?;
        config.alpn_protocols = alpn_protocols;
    } else {
        // Default ALPN: http/1.1
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
    }

    Ok(config)
}

/// Extract ALPN protocol list from a Seq of Strings
fn extract_alpn_protocols(
    val: &Value,
    span: Span,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<Vec<Vec<u8>>> {
    let mut protocols = Vec::new();
    let mut current = val.clone();

    loop {
        match current {
            Value::Dict(ref d) if d.is_empty() => break, // Null (end of list)
            Value::Seq { head, tail } => {
                // Materialize head and tail
                let head_thunk = ctx.get_thunk(head);
                let head_val = materialize(&head_thunk, Some(&span), ctx)?;

                let protocol_str = match head_val {
                    Value::String { source, start, end } => source[start..end].to_string(),
                    other => {
                        return Err(EvalError::type_mismatch_ctx(
                            "tls-connect opts.alpn".to_string(),
                            "Seq of String",
                            other.type_name(),
                            span,
                        )
                        .into())
                    }
                };
                protocols.push(protocol_str.into_bytes());

                let tail_thunk = ctx.get_thunk(tail);
                current = materialize(&tail_thunk, Some(&span), ctx)?;
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "tls-connect opts.alpn".to_string(),
                    "Seq of String",
                    other.type_name(),
                    span,
                )
                .into())
            }
        }
    }

    Ok(protocols)
}

/// Slurp a Handle into bytes (for reading PEM files)
fn slurp_handle_bytes(val: &Value, span: Span) -> EvalResult<Vec<u8>> {
    match val {
        Value::Handle { inner, .. } => {
            use std::io::Read;
            let mut bytes = Vec::new();
            inner.borrow_mut().read_to_end(&mut bytes).map_err(|e| {
                EvalError::user_error(format!("tls-connect: failed to read Handle: {}", e), span)
            })?;
            Ok(bytes)
        }
        other => Err(EvalError::type_mismatch_ctx(
            "tls-connect opts.ca-bundle/client-cert/client-key".to_string(),
            "Handle",
            other.type_name(),
            span,
        )
        .into()),
    }
}

/// Validate SPKI pins against the peer certificate
fn validate_spki_pins(
    conn: &rustls::ClientConnection,
    pins_val: &Value,
    span: Span,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<()> {
    // Extract list of pins
    let mut pins = Vec::new();
    let mut current = pins_val.clone();

    loop {
        match current {
            Value::Dict(ref d) if d.is_empty() => break, // Null (end of list)
            Value::Seq { head, tail } => {
                let head_thunk = ctx.get_thunk(head);
                let pin_val = materialize(&head_thunk, Some(&span), ctx)?;
                pins.push(pin_val);

                let tail_thunk = ctx.get_thunk(tail);
                current = materialize(&tail_thunk, Some(&span), ctx)?;
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "tls-connect opts.pins".to_string(),
                    "Seq of SpkiPin",
                    other.type_name(),
                    span,
                )
                .into())
            }
        }
    }

    if pins.is_empty() {
        return Ok(()); // No pins to validate
    }

    // Get leaf certificate
    let peer_certs = conn.peer_certificates().ok_or_else(|| {
        EvalError::user_error(
            "tls-connect: no peer certificates available for SPKI pin validation".to_string(),
            span,
        )
    })?;

    if peer_certs.is_empty() {
        return Err(EvalError::user_error(
            "tls-connect: peer certificate list is empty".to_string(),
            span,
        )
        .into());
    }

    let leaf_cert = &peer_certs[0];

    // Extract SPKI from certificate and compute hashes
    // For simplicity, we'll compute the hash of the entire DER-encoded certificate's SPKI field
    // This requires parsing the certificate, which is complex
    // For now, we'll use a simpler approach: hash the raw certificate bytes
    // TODO: Properly extract SPKI field from X.509 certificate

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
                    span,
                )
                .into())
            }
        };

        let algorithm_thunk_id = pin_dict
            .get(&crate::value::Key::String("algorithm".to_string()))
            .ok_or_else(|| {
                EvalError::user_error(
                    "tls-connect: SpkiPin missing 'algorithm' field".to_string(),
                    span,
                )
            })?;
        let algorithm_thunk = ctx.get_thunk(*algorithm_thunk_id);
        let algorithm_val = materialize(&algorithm_thunk, Some(&span), ctx)?;

        let fingerprint_thunk_id = pin_dict
            .get(&crate::value::Key::String("fingerprint".to_string()))
            .ok_or_else(|| {
                EvalError::user_error(
                    "tls-connect: SpkiPin missing 'fingerprint' field".to_string(),
                    span,
                )
            })?;
        let fingerprint_thunk = ctx.get_thunk(*fingerprint_thunk_id);
        let fingerprint_val = materialize(&fingerprint_thunk, Some(&span), ctx)?;

        let algorithm_tag = match algorithm_val {
            Value::Variant { tag, .. } => tag,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "tls-connect opts.pins.algorithm".to_string(),
                    "HashAlgorithm variant",
                    other.type_name(),
                    span,
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
                    span,
                )
                .into())
            }
        };

        // Compute hash of certificate using the specified algorithm
        // Note: This is a simplified implementation that hashes the whole cert
        // A proper implementation would extract and hash only the SPKI field
        let computed_hash = compute_spki_hash(leaf_cert.as_ref(), &algorithm_tag, span)?;

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
            span,
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
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<Value> {
    // For now, return a minimal dict with just the cert bytes
    // Full X.509 parsing would require a crate like x509-parser or rustls-webpki
    let mut info = IndexMap::new();

    // Store the raw cert DER bytes so tls-peer-cert can parse it later
    use crate::value::Key;
    info.insert(
        Key::String("_raw_der".to_string()),
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
fn extract_sans(
    cert: &x509_parser::certificate::X509Certificate,
    span: Span,
    ctx: &Rc<crate::eval::EvalContext>,
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

    // Convert Vec<Value> to a Seq by building from right to left
    // End of Seq is an empty Dict
    let mut result = ctx.alloc_thunk(ok_val(Value::Dict(IndexMap::new()), span)?);
    for val in sans_list.into_iter().rev() {
        let head_thunk = ctx.alloc_thunk(ok_val(val, span)?);
        result = ctx.alloc_thunk(ok_val(
            Value::Seq {
                head: head_thunk,
                tail: result,
            },
            span,
        )?);
    }

    // Materialize the final Seq
    materialize(&ctx.get_thunk(result), Some(&span), ctx)
}

/// `tls-layer`: Layer TLS on an existing TCP Handle (STARTTLS use case).
/// Takes (handle, sni, opts). Extracts raw_tcp from Handle, wraps in TLS, returns new Handle.
/// Signature: tls-layer handle@Handle sni@String opts@Dict → Handle[... Stream Tls]
pub(crate) fn builtin_tls_layer(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 args: handle, sni, opts
    if args.len() != 3 {
        return Err(EvalError::user_error(
            format!(
                "tls-layer: expected 3 arguments (handle sni opts), got {}",
                args.len()
            ),
            call_span,
        )
        .into());
    }
    reject_named("tls-layer", named, call_span)?;

    let handle_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let sni_val = materialize(&args[1], Some(&call_span), &ctx)?;
    let opts_val = materialize(&args[2], Some(&call_span), &ctx)?;

    let sni = require_string("tls-layer", sni_val, args[1].span)?;

    // Extract Handle and its raw_tcp
    let (raw_tcp_slot, caps, creation_span) = match handle_val {
        Value::Handle {
            raw_tcp: Some(slot),
            caps,
            creation_span,
            ..
        } => (slot, caps, creation_span),
        Value::Handle {
            raw_tcp: None,
            creation_span,
            ..
        } => {
            // Dual-span error: call_span (primary) + creation_span (secondary)
            return Err(EvalError::user_error(
                "tls-layer: handle does not have a raw TCP stream (not created by connect cap Tcp)"
                    .to_string(),
                call_span,
            )
            .with_secondary_span(creation_span, "handle created here")
            .into());
        }
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "tls-layer".to_string(),
                "Handle",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    // Check Handle has Stream capability
    if !caps.contains_key("Stream") {
        return Err(EvalError::user_error(
            "tls-layer: handle must have Stream capability".to_string(),
            args[0].span,
        )
        .into());
    }

    // Take the TcpStream from the shared slot (invalidates all aliases)
    let tcp_stream = raw_tcp_slot.borrow_mut().take().ok_or_else(|| {
        // Dual-span error: call_span (primary) + creation_span (secondary)
        EvalError::user_error(
            "tls-layer: raw TCP stream already consumed by a previous tls-layer call".to_string(),
            call_span,
        )
        .with_secondary_span(creation_span, "handle created here")
    })?;

    // Build TLS config
    let tls_config = build_tls_config(&opts_val, args[2].span, &ctx)?;

    // Create TLS connection
    let server_name = rustls::pki_types::ServerName::try_from(sni.clone())
        .map_err(|e| {
            EvalError::user_error(
                format!("tls-layer: invalid server name '{}': {}", sni, e),
                args[1].span,
            )
        })?
        .to_owned();

    let client_conn = rustls::ClientConnection::new(std::sync::Arc::new(tls_config), server_name)
        .map_err(|e| {
        EvalError::user_error(
            format!("tls-layer: failed to create TLS connection: {}", e),
            call_span,
        )
    })?;

    let tls_stream = rustls::StreamOwned::new(client_conn, tcp_stream);
    let shared_stream = Rc::new(RefCell::new(tls_stream));

    // Perform TLS handshake by attempting to flush
    {
        use std::io::Write;
        shared_stream.borrow_mut().flush().map_err(|e| {
            EvalError::user_error(format!("tls-layer: TLS handshake failed: {}", e), call_span)
        })?;
    }

    // Validate SPKI pins if provided
    if let Value::Dict(opts_map) = &opts_val {
        if let Some(pins_thunk_id) = opts_map.get(&crate::value::Key::String("pins".to_string())) {
            let pins_thunk = ctx.get_thunk(*pins_thunk_id);
            let pins_val = materialize(&pins_thunk, Some(&call_span), &ctx)?;
            validate_spki_pins(&shared_stream.borrow().conn, &pins_val, call_span, &ctx)?;
        }
    }

    // Extract peer certificate info for the Tls capability
    let tls_info = {
        let stream_borrow = shared_stream.borrow();
        let peer_certs = stream_borrow.conn.peer_certificates();
        if let Some(certs) = peer_certs {
            if !certs.is_empty() {
                // Clone the cert DER bytes before dropping the borrow
                let cert_der = certs[0].clone();
                drop(stream_borrow);
                extract_cert_info(&cert_der, call_span, &ctx)?
            } else {
                Value::Dict(IndexMap::new()) // No cert
            }
        } else {
            Value::Dict(IndexMap::new()) // No cert
        }
    };

    // Create read and write wrappers
    let reader = TlsReader {
        stream: Rc::clone(&shared_stream),
        buf: Vec::new(),
        buf_pos: 0,
    };
    let writer = TlsWriter {
        stream: Rc::clone(&shared_stream),
    };

    let inner = Rc::new(RefCell::new(Box::new(reader) as Box<dyn std::io::BufRead>));
    let write_inner = Some(Rc::new(RefCell::new(
        Box::new(writer) as Box<dyn std::io::Write>
    )));

    // Build capabilities: Binary Readable Writable Stream Tls
    let mut new_caps = HashMap::new();
    new_caps.insert("Readable".to_string(), Value::Dict(IndexMap::new()));
    new_caps.insert("Writable".to_string(), Value::Dict(IndexMap::new()));
    new_caps.insert("Binary".to_string(), Value::Dict(IndexMap::new()));
    new_caps.insert("Stream".to_string(), Value::Dict(IndexMap::new()));
    new_caps.insert("Tls".to_string(), tls_info);

    ok_val(
        Value::Handle {
            caps: new_caps,
            inner,
            write_inner,
            seek_inner: None,
            raw_tcp: None, // Consumed by this operation
            creation_span: call_span,
        },
        call_span,
    )
}

/// `tls-peer-cert`: Extract TLS certificate metadata from a TLS handle.
/// Requires Handle[... Tls ...].
/// Returns a dict with: subject, issuer, sans, not-before, not-after, spki-sha256.
pub(crate) fn builtin_tls_peer_cert(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    let val = crate::builtins::expect_one_arg("tls-peer-cert", args, named, &ctx, call_span)?;

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

    let tls_info = caps.get("Tls").ok_or_else(|| {
        EvalError::user_error(
            "tls-peer-cert: handle must have Tls capability (created by tls-connect)".to_string(),
            call_span,
        )
    })?;

    // The TlsInfo is stored in the Tls capability — it's a dict with _raw_der
    match tls_info {
        Value::Dict(dict) => {
            use crate::value::Key;

            // Extract the _raw_der bytes from the dict
            let raw_der_thunk_id =
                dict.get(&Key::String("_raw_der".to_string()))
                    .ok_or_else(|| {
                        EvalError::user_error(
                            "tls-peer-cert: TLS capability missing _raw_der field".to_string(),
                            call_span,
                        )
                    })?;

            // Get the thunk and materialize it
            let raw_der_thunk = ctx.get_thunk(*raw_der_thunk_id);
            let raw_der_val = materialize(&raw_der_thunk, Some(&call_span), &ctx)?;
            let cert_der = match &raw_der_val {
                Value::Bytes { source, start, end } => &source[*start..*end],
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "tls-peer-cert".to_string(),
                        "Bytes",
                        other.type_name(),
                        call_span,
                    )
                    .into())
                }
            };

            // Parse the X.509 certificate
            let (_, cert) = x509_parser::parse_x509_certificate(cert_der).map_err(|e| {
                EvalError::user_error(
                    format!("tls-peer-cert: failed to parse certificate: {}", e),
                    call_span,
                )
            })?;

            // Extract subject CN (Common Name)
            let subject = extract_cn(&cert.tbs_certificate.subject).unwrap_or("(none)".to_string());

            // Extract issuer CN
            let issuer = extract_cn(&cert.tbs_certificate.issuer).unwrap_or("(none)".to_string());

            // Extract validity dates (convert to Unix timestamps)
            let not_before = cert.tbs_certificate.validity.not_before.timestamp();
            let not_after = cert.tbs_certificate.validity.not_after.timestamp();

            // Extract SANs (Subject Alternative Names)
            let sans = extract_sans(&cert, call_span, &ctx)?;

            // Compute SPKI SHA-256 hash
            let spki_der = cert.tbs_certificate.subject_pki.raw;
            let spki_hash = {
                use sha2::{Digest, Sha256};
                Sha256::digest(spki_der)
            };
            let spki_hex = hex::encode(spki_hash);

            // Build the result dict
            let mut cert_info = IndexMap::new();
            cert_info.insert(
                Key::String("subject".to_string()),
                ctx.alloc_thunk(ok_val(string_val(&subject), call_span)?),
            );
            cert_info.insert(
                Key::String("issuer".to_string()),
                ctx.alloc_thunk(ok_val(string_val(&issuer), call_span)?),
            );
            cert_info.insert(
                Key::String("sans".to_string()),
                ctx.alloc_thunk(ok_val(sans, call_span)?),
            );
            cert_info.insert(
                Key::String("not-before".to_string()),
                ctx.alloc_thunk(ok_val(Value::Int(not_before), call_span)?),
            );
            cert_info.insert(
                Key::String("not-after".to_string()),
                ctx.alloc_thunk(ok_val(Value::Int(not_after), call_span)?),
            );
            cert_info.insert(
                Key::String("spki-sha256".to_string()),
                ctx.alloc_thunk(ok_val(string_val(&spki_hex), call_span)?),
            );

            ok_val(Value::Dict(cert_info), call_span)
        }
        other => Err(EvalError::type_mismatch_ctx(
            "tls-peer-cert".to_string(),
            "TlsInfo dict",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// `spki-pin`: Create an SPKI pin dict.
/// Takes HashAlgorithm variant and Bytes fingerprint.
/// Returns dict: {algorithm: Variant, fingerprint: Bytes}.
pub(crate) fn builtin_spki_pin(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named("spki-pin", named, call_span)?;

    let algorithm_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let fingerprint_val = materialize(&args[1], Some(&call_span), &ctx)?;

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

/// `http-get`: Make an HTTP GET request.
/// Overloaded form: http-get conn@HttpConn path@String [headers@Dict]
/// Returns a Dict with status, headers, and body.
pub(crate) fn builtin_http_get(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    if args.len() < 2 || args.len() > 3 {
        return Err(EvalError::user_error(
            format!(
                "http-get: expected 2 or 3 arguments (conn path [headers]), got {}",
                args.len()
            ),
            call_span,
        )
        .into());
    }
    reject_named("http-get", named, call_span)?;

    let conn_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let path_val = materialize(&args[1], Some(&call_span), &ctx)?;

    // Extract HttpConn
    let (client, base_url) = match conn_val {
        Value::HttpConn { client, base_url } => (client, base_url),
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "http-get".to_string(),
                "HttpConn",
                other.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    // Extract path
    let path = require_string("http-get", path_val, args[1].span)?;

    // Build URL
    let url = if let Some(base) = base_url {
        // Append path to base URL
        if path.starts_with('/') {
            format!("{}{}", base.trim_end_matches('/'), path)
        } else {
            format!("{}/{}", base.trim_end_matches('/'), path)
        }
    } else {
        path
    };

    // Make the request
    let response = client.get(&url).send().map_err(|e| {
        EvalError::user_error(format!("http-get: request failed: {}", e), call_span)
    })?;

    // Extract status
    let status = response.status().as_u16() as i64;

    // Extract headers as a dict
    let mut headers_dict = IndexMap::new();
    for (name, value) in response.headers().iter() {
        let key = crate::value::Key::String(name.as_str().to_string());
        let value_str = value.to_str().unwrap_or("<invalid UTF-8>").to_string();
        headers_dict.insert(
            key,
            ctx.alloc_thunk(ok_val(string_val(&value_str), call_span)?),
        );
    }

    // Extract body as bytes
    let body_bytes = response.bytes().map_err(|e| {
        EvalError::user_error(
            format!("http-get: failed to read response body: {}", e),
            call_span,
        )
    })?;

    // Build result dict
    use crate::value::Key;
    let mut result = IndexMap::new();
    result.insert(
        Key::String("status".to_string()),
        ctx.alloc_thunk(ok_val(Value::Int(status), call_span)?),
    );
    result.insert(
        Key::String("headers".to_string()),
        ctx.alloc_thunk(ok_val(Value::Dict(headers_dict), call_span)?),
    );
    result.insert(
        Key::String("body".to_string()),
        ctx.alloc_thunk(ok_val(
            Value::Bytes {
                source: Rc::from(body_bytes.as_ref()),
                start: 0,
                end: body_bytes.len(),
            },
            call_span,
        )?),
    );

    ok_val(Value::Dict(result), call_span)
}

/// `socks5-connect`: Create a SOCKS5 proxy tunnel.
/// Stub implementation — returns error "not yet implemented".
pub(crate) fn builtin_socks5_connect(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs { call_span, .. } = ctx_arg;
    Err(EvalError::user_error("socks5-connect: not yet implemented".to_string(), call_span).into())
}

/// `proxy-connect`: Create an HTTP CONNECT proxy tunnel.
/// Stub implementation — returns error "not yet implemented".
pub(crate) fn builtin_proxy_connect(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs { call_span, .. } = ctx_arg;
    Err(EvalError::user_error("proxy-connect: not yet implemented".to_string(), call_span).into())
}
