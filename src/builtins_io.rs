//! Filesystem and network I/O builtins: dir-cap, open, slurp, write, write-atomic, connect, lines.
//!
//! These builtins provide capability-based access to filesystems and networks,
//! implementing object-capability security through DirCap and NetCap values.
//!
//! **Filesystem builtins:**
//! - `dir-cap`: Create a DirCap from a path
//! - `open`: Open a file within a DirCap
//! - `slurp`: Read all bytes from a Handle
//! - `write`: Write a string to a file
//! - `write-atomic`: Atomically write to a file (temp + rename)
//! - `narrow`: Attenuate a DirCap to a subdirectory
//! - `revocable`: Wrap a DirCap in a revocable wrapper
//! - `revoke-cap`: Revoke a RevocableDirCap
//!
//! **Network builtins:**
//! - `net-cap`: Create a NetCap from an allowlist
//! - `connect`: Open a TCP connection within a NetCap
//!
//! **I/O helpers:**
//! - `lines`: Read lines from a Handle lazily
//! - `emit`: Write to stdout and suppress JSON output
//! - `env`: Read environment variables
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration in `standard_builtins()` and `create_root_env()` remains in `builtins.rs`.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::{builtin, ok_val, reject_named, require_string};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::value::{BuiltinArgs, Thunk, Value};

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
        Ok(value) => ok_val(Value::String(value), call_span),
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

    ok_val(
        Value::Handle(Rc::new(std::cell::RefCell::new(handle))),
        call_span,
    )
}

/// `slurp`: Read all bytes from a Handle to a String.
/// Takes a Handle, reads to EOF, returns String.
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
    let handle = match val {
        Value::Handle(h) => h,
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

    // Read to string
    use std::io::Read;
    let mut contents = String::new();
    handle
        .borrow_mut()
        .read_to_string(&mut contents)
        .map_err(|e| EvalError::user_error(format!("slurp: read failed: {}", e), call_span))?;

    ok_val(Value::String(contents), call_span)
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
        Value::String(s) => {
            // Single string — wrap in vec
            entries.push(parse_net_cap_entry(&s, call_span)?);
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

/// `connect`: Open a TCP connection within a NetCap.
/// Takes a NetCap, hostname String, and port Int.
/// Returns Value::Handle wrapping a BufReader<TcpStream>.
pub(crate) fn builtin_connect(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect 3 args: NetCap, String host, Int port
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    reject_named("connect", named, call_span)?;

    let cap_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let host_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;
    let port_val = materialize(&args[2], Some(&call_span), &ctx, depth)?;

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

    // Wrap in BufReader for Handle
    let buf_reader = std::io::BufReader::new(stream);
    let handle = Rc::new(RefCell::new(
        Box::new(buf_reader) as Box<dyn std::io::BufRead>
    ));

    ok_val(Value::Handle(handle), call_span)
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
    let handle = match val {
        Value::Handle(h) => h,
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

    // Wrap the Handle in a new Handle that the step function can use
    // The step function will read one line, then return a Seq with the line as head
    // and a PendingBuiltin thunk for the next line as tail
    builtin_lines_step(handle, depth, call_span, ctx)
}

/// Helper for `lines`: reads one line and returns Seq or null.
pub(crate) fn builtin_lines_step(
    handle: Rc<RefCell<Box<dyn std::io::BufRead>>>,
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
            let head = ok_val(Value::String(line), call_span)?;
            let head_id = ctx.alloc_thunk(head);

            // Create tail as PendingBuiltin that will read the next line
            // We need to pass the Handle through to the next step
            let tail_args = vec![ok_val(Value::Handle(Rc::clone(&handle)), call_span)?];
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
