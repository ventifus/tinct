//! Filesystem I/O builtins: open, write, write-atomic, builtin-read-line, builtin-read-chunk, builtin-read-all.
//!
//! These builtins provide capability-based access to filesystems,
//! implementing object-capability security through DirCap values.
//!
//! **Filesystem builtins:**
//! - `open`: Open a file within a DirCap
//! - `write`: Write a string to a file (DirCap-based)
//! - `write-atomic`: Atomically write to a file (temp + rename)
//! - `narrow`: Attenuate a DirCap to a subdirectory
//! - `revocable`: Wrap a DirCap in a revocable wrapper
//! - `revoke-cap`: Revoke a RevocableDirCap
//!
//! **Handle capability builtins:**
//! - `cap-data`: Extract capability data from a Handle/WriteHandle (returns Null on miss)
//! - `has-cap?`: Check if a capability is present on a Handle/WriteHandle (implemented in stdlib/io.llt)
//! - `write-handle`: Write to a WriteHandle (returns handle for chaining)
//! - `flush`: Flush a WriteHandle buffer
//! - `close`: Close a WriteHandle
//!
//! **I/O helpers:**
//! - `builtin-read-line`: Read a single line from a Handle (Text mode, returns String or [] on EOF)
//! - `builtin-read-chunk`: Read n bytes from a Handle (returns Bytes or [] on EOF)
//! - `builtin-read-all`: Read all bytes from a Handle to EOF, returns as String
//! - `emit`: Write to stdout and suppress JSON output
//! - `env`: Read environment variables
//!
//! **Note:** `builtin-read-line` and `builtin-read-chunk` are synchronous builtins that use
//! `BufRead::read_line()` and `Read::read()` respectively. They are safe for the tinct lazy
//! evaluation model, but when called from within an async context (e.g., inside a `[task ...]`),
//! they block the current thread. For large files, use explicit `[collect [lines handle]]`
//! patterns outside task boundaries.
//!
//! Network builtins were moved to `builtins_net.rs` in T-915.
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration is via `core_builtins()` in `src/builtins_core.rs`, dispatched by
//! `builtin_module("core")` in `src/builtins.rs`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::io::BufReader;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::{ok_val, reject_named, require_string};
use crate::error::{EvalError, EvalResult};
use crate::value::{string_val, BuiltinArgs, DirPerms, Thunk, Value};

/// Extract DirCap from a Value, checking revocation and returning (dir, perms).
/// Used by all DirCap-consuming builtins.
pub(crate) fn extract_dir_cap<'a>(
    val: &'a Value,
    builtin_name: &str,
    span: Span,
) -> EvalResult<(&'a Rc<cap_std::fs::Dir>, &'a DirPerms)> {
    match val {
        Value::DirCap { dir, perms } => Ok((dir, perms)),
        Value::RevocableDirCap {
            inner,
            perms,
            revoked,
        } => {
            if revoked.get() {
                return Err(EvalError::user_error(
                    format!("{builtin_name}: capability has been revoked"),
                    span,
                )
                .into());
            }
            Ok((inner, perms))
        }
        other => Err(EvalError::type_mismatch_ctx(
            builtin_name.to_string(),
            "DirCap",
            other.type_name(),
            span,
        )
        .into()),
    }
}

/// Check if a DirCap has the required permission flag.
fn check_perm(
    _perms: &DirPerms,
    perm_name: &str,
    perm_value: bool,
    _builtin_name: &str,
    span: Span,
) -> EvalResult<()> {
    if !perm_value {
        return Err(EvalError::user_error(
            format!("DirCap: operation requires {perm_name} permission"),
            span,
        )
        .into());
    }
    Ok(())
}

/// `emit`: Write a string to stdout.
/// Takes a String argument, writes it to stdout, returns null (empty dict).
pub(crate) fn builtin_emit(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = crate::builtins::expect_one_arg(
            "emit",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let s = require_string("emit", val, args[0].span.clone())?;

        // Write to stdout
        use std::io::Write;
        std::io::stdout()
            .write_all(s.as_bytes())
            .map_err(|e| EvalError::user_error(format!("emit failed: {e}"), call_span.clone()))?;

        // Return null (empty dict)
        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `env`: Read an environment variable by name.
/// Returns the value as a String, or `Absent.Absent` if not set or not allowed.
/// Gated by ctx.env_allowed: None = all denied, Some(set) = only those allowed.
pub(crate) fn builtin_env(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val =
            crate::builtins::expect_one_arg("env", &args, named.as_ref(), &ctx, call_span.clone())?;
        let name = require_string("env", val, args[0].span.clone())?;

        // Check env_allowed
        // None = unrestricted (all allowed), Some(set) = only those in the set
        let allowed = match &ctx.env_allowed {
            None => true, // None means unrestricted access
            Some(set) => set.contains(&name),
        };

        if !allowed {
            // Return Absent.Absent if not allowed
            return ok_val(
                Value::Variant {
                    tag: "Absent.Absent".into(),
                    payload: None,
                },
                call_span,
            );
        }

        // Read env var
        match std::env::var(name) {
            Ok(value) => ok_val(string_val(&value), call_span),
            Err(_) => ok_val(
                Value::Variant {
                    tag: "Absent.Absent".into(),
                    payload: None,
                },
                call_span,
            ), // Not set -> Absent.Absent
        }
    })
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
pub(crate) fn builtin_open(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        // Require at least 3 args: DirCap, String path, and at least one flag/mode
        if args.len() < 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span.clone()).into());
        }
        reject_named("open", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[1]=Seq");

        // Extract DirCap and permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "open", args[0].span.clone())?;

        let path = require_string("open", path_val, args[1].span.clone())?;

        // Parse flags from args[2..]
        let mut caps = HashMap::new();
        let mut has_readable = false;
        let mut has_writable = false;
        let mut has_appendable = false;
        let mut has_binary = false;
        let mut has_text = false;
        let mut has_seekable = false;

        for flag_arg in &args[2..] {
            let flag_val = crate::eval::materialize(flag_arg, Some(&call_span), &ctx).await?;

            match flag_val {
                Value::Variant { ref tag, .. } => {
                    // Strip qualifier prefix ("OpenFlag.Readable" → "Readable") for
                    // compatibility with T-974 qualified variant tags.
                    let flag_name = tag.strip_prefix("OpenFlag.").unwrap_or(tag.as_str());
                    match flag_name {
                        "Readable" => {
                            if has_writable || has_appendable {
                                return Err(EvalError::user_error(
                                "open: cannot specify Readable with Writable or Appendable flags"
                                    .to_string(),
                                call_span.clone(),
                            )
                            .into());
                            }
                            has_readable = true;
                            caps.insert("Readable".to_string(), Value::Bool(true));
                        }
                        "Writable" => {
                            if has_readable {
                                return Err(EvalError::user_error(
                                    "open: cannot specify both Readable and Writable flags"
                                        .to_string(),
                                    call_span.clone(),
                                )
                                .into());
                            }
                            has_writable = true;
                            caps.insert("Writable".to_string(), Value::Bool(true));
                        }
                        "Appendable" => {
                            if has_readable {
                                return Err(EvalError::user_error(
                                    "open: cannot specify both Readable and Appendable flags"
                                        .to_string(),
                                    call_span.clone(),
                                )
                                .into());
                            }
                            has_appendable = true;
                            caps.insert("Appendable".to_string(), Value::Bool(true));
                        }
                        "Binary" => {
                            if has_text {
                                return Err(EvalError::user_error(
                                    "open: cannot specify both Binary and Text flags".to_string(),
                                    call_span.clone(),
                                )
                                .into());
                            }
                            has_binary = true;
                            caps.insert("Binary".to_string(), Value::Bool(true));
                        }
                        "Text" => {
                            if has_binary {
                                return Err(EvalError::user_error(
                                    "open: cannot specify both Binary and Text flags".to_string(),
                                    call_span.clone(),
                                )
                                .into());
                            }
                            has_text = true;
                            caps.insert("Text".to_string(), Value::Bool(true));
                        }
                        "Seekable" => {
                            has_seekable = true;
                            caps.insert("Seekable".to_string(), Value::Bool(true));
                        }
                        other => {
                            return Err(EvalError::user_error(
                            format!(
                                "open: unknown capability flag '{}' (expected Readable, Writable, Appendable, Binary, Text, or Seekable)",
                                other
                            ),
                            call_span.clone(),
                        )
                        .into());
                        }
                    } // close match flag_name
                }
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "open".to_string(),
                        "Variant (capability flag)",
                        other.type_name(),
                        flag_arg.span.clone(),
                    )
                    .into());
                }
            }
        }

        // Require at least one of Readable, Writable, or Appendable
        if !has_readable && !has_writable && !has_appendable {
            return Err(EvalError::user_error(
                "open: must specify at least one of Readable, Writable, or Appendable flags"
                    .to_string(),
                call_span.clone(),
            )
            .into());
        }

        // Check DirCap permissions based on mode
        if has_readable {
            check_perm(perms, "Readable", perms.readable, "open", call_span.clone())?;
        }
        if has_writable {
            check_perm(perms, "Writable", perms.writable, "open", call_span.clone())?;
        }
        if has_appendable {
            check_perm(
                perms,
                "Appendable",
                perms.appendable,
                "open",
                call_span.clone(),
            )?;
        }

        // Default to Text encoding if neither Binary nor Text specified
        if !has_binary && !has_text {
            caps.insert("Text".to_string(), Value::Bool(true));
        }

        // Open the file based on flags
        use cap_std::fs::OpenOptions;
        use std::io::{BufReader, BufWriter};
        if has_readable {
            // Read mode
            let file = dir.open(&path).map_err(|e| {
                EvalError::user_error(
                    format!("open: failed to open file '{}': {}", path, e),
                    call_span.clone(),
                )
            })?;

            // If Seekable, clone the file handle for seeking operations
            // We need two handles: one wrapped in BufReader for reading, one for seeking
            let seek_inner = if has_seekable {
                let seek_file = file.try_clone().map_err(|e| {
                    EvalError::user_error(
                        format!("open: failed to clone file handle for seeking: {}", e),
                        call_span.clone(),
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
                    creation_span: call_span.clone(),
                },
                call_span,
            )
        } else if has_writable {
            // Write mode: create/truncate
            let file = dir
                .open_with(
                    &path,
                    OpenOptions::new().write(true).create(true).truncate(true),
                )
                .map_err(|e| {
                    EvalError::user_error(
                        format!("open: failed to open file '{}' for writing: {}", path, e),
                        call_span.clone(),
                    )
                })?;

            let writer: Box<dyn std::io::Write> = Box::new(BufWriter::new(file));

            ok_val(
                Value::WriteHandle {
                    caps,
                    inner: Rc::new(std::cell::RefCell::new(writer)),
                },
                call_span,
            )
        } else if has_appendable {
            // Append mode: append/create
            let file = dir
                .open_with(&path, OpenOptions::new().append(true).create(true))
                .map_err(|e| {
                    EvalError::user_error(
                        format!("open: failed to open file '{}' for appending: {}", path, e),
                        call_span.clone(),
                    )
                })?;

            let writer: Box<dyn std::io::Write> = Box::new(BufWriter::new(file));

            ok_val(
                Value::WriteHandle {
                    caps,
                    inner: Rc::new(std::cell::RefCell::new(writer)),
                },
                call_span,
            )
        } else {
            // Should never reach here due to earlier validation
            Err(EvalError::user_error(
                "open: internal error - no mode specified".to_string(),
                call_span,
            )
            .into())
        }
    })
}

/// `narrow`: Attenuate a DirCap to a subdirectory or restrict permissions.
/// Two forms:
///   [narrow cap "path"] — restrict to subdirectory, preserve permissions
///   [narrow cap FlagName...] — restrict permissions (intersection), preserve directory
/// Takes a DirCap and either:
///   - A String subpath to narrow to a subdirectory (preserves permissions)
///   - One or more Variant flags to restrict permissions (preserves directory)
///
///     Returns a new DirCap with the narrowed scope or restricted permissions.
pub(crate) fn builtin_narrow(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        // Expect at least 2 args: DirCap, and either a subpath string or flag(s)
        if args.len() < 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("narrow", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");

        // Extract DirCap and current permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "narrow", args[0].span.clone())?;

        // Check if second arg is a String (subtree narrowing) or Variant (permission restriction)
        let second_arg_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[1]=Seq");

        if matches!(second_arg_val, Value::String { .. }) {
            // Subtree narrowing: [narrow cap "path"]
            if args.len() != 2 {
                return Err(EvalError::user_error(
                    "narrow: subtree mode requires exactly 2 arguments (cap, subpath)".to_string(),
                    call_span,
                )
                .into());
            }

            let subpath = require_string("narrow", second_arg_val, args[1].span.clone())?;

            // Open subdirectory (RESOLVE_BENEATH applies to subpath)
            let narrowed = dir.open_dir(&subpath).map_err(|e| {
                EvalError::user_error(
                    format!("narrow: failed to open subdirectory '{}': {}", subpath, e),
                    call_span.clone(),
                )
            })?;

            ok_val(
                Value::DirCap {
                    dir: Rc::new(narrowed),
                    perms: perms.clone(),
                },
                call_span,
            )
        } else if matches!(second_arg_val, Value::Variant { .. }) {
            // Permission restriction: [narrow cap FlagName...]
            // Parse requested flags from args[1..]
            let mut requested = DirPerms {
                readable: false,
                statable: false,
                listable: false,
                writable: false,
                appendable: false,
                deletable: false,
                renameable: false,
                symlinkable: false,
                posix_permissions: false,
                extended_attributes: false,
            };

            for flag_arg in &args[1..] {
                let flag_val = crate::eval::materialize(flag_arg, Some(&call_span), &ctx).await?;

                match flag_val {
                    Value::Variant { ref tag, .. } => {
                        // Strip qualifier prefix ("DirCapFlag.Statable" → "Statable") for
                        // compatibility with T-974 qualified variant tags.
                        let flag_name = tag.rfind('.').map_or(tag.as_str(), |pos| &tag[pos + 1..]);
                        match flag_name {
                            "Readable" => requested.readable = true,
                            "Statable" => requested.statable = true,
                            "Listable" => requested.listable = true,
                            "Writable" => requested.writable = true,
                            "Appendable" => requested.appendable = true,
                            "Deletable" => requested.deletable = true,
                            "Renameable" => requested.renameable = true,
                            "Symlinkable" => requested.symlinkable = true,
                            "PosixPermissions" => requested.posix_permissions = true,
                            "ExtendedAttributes" => requested.extended_attributes = true,
                            other => {
                                return Err(EvalError::user_error(
                            format!(
                                "narrow: unknown capability flag '{}' (expected Readable, Statable, Listable, Writable, Appendable, Deletable, Renameable, Symlinkable, PosixPermissions, ExtendedAttributes)",
                                other
                            ),
                            call_span.clone(),
                        )
                        .into());
                            }
                        }
                    }
                    other => {
                        return Err(EvalError::type_mismatch_ctx(
                            "narrow".to_string(),
                            "Variant (capability flag) or String (subpath)",
                            other.type_name(),
                            flag_arg.span.clone(),
                        )
                        .into());
                    }
                }
            }

            // Compute intersection: only grant flags that are BOTH requested AND held by source
            let narrowed_perms = DirPerms {
                readable: requested.readable && perms.readable,
                statable: requested.statable && perms.statable,
                listable: requested.listable && perms.listable,
                writable: requested.writable && perms.writable,
                appendable: requested.appendable && perms.appendable,
                deletable: requested.deletable && perms.deletable,
                renameable: requested.renameable && perms.renameable,
                symlinkable: requested.symlinkable && perms.symlinkable,
                posix_permissions: requested.posix_permissions && perms.posix_permissions,
                extended_attributes: requested.extended_attributes && perms.extended_attributes,
            };

            // Runtime error if a requested flag is not held in the source
            if requested.readable && !perms.readable {
                return Err(EvalError::user_error(
                    "narrow: source DirCap does not have Readable permission".to_string(),
                    call_span.clone(),
                )
                .into());
            }
            if requested.statable && !perms.statable {
                return Err(EvalError::user_error(
                    "narrow: source DirCap does not have Statable permission".to_string(),
                    call_span.clone(),
                )
                .into());
            }
            if requested.listable && !perms.listable {
                return Err(EvalError::user_error(
                    "narrow: source DirCap does not have Listable permission".to_string(),
                    call_span.clone(),
                )
                .into());
            }
            if requested.writable && !perms.writable {
                return Err(EvalError::user_error(
                    "narrow: source DirCap does not have Writable permission".to_string(),
                    call_span.clone(),
                )
                .into());
            }
            if requested.appendable && !perms.appendable {
                return Err(EvalError::user_error(
                    "narrow: source DirCap does not have Appendable permission".to_string(),
                    call_span.clone(),
                )
                .into());
            }
            if requested.deletable && !perms.deletable {
                return Err(EvalError::user_error(
                    "narrow: source DirCap does not have Deletable permission".to_string(),
                    call_span.clone(),
                )
                .into());
            }
            if requested.renameable && !perms.renameable {
                return Err(EvalError::user_error(
                    "narrow: source DirCap does not have Renameable permission".to_string(),
                    call_span.clone(),
                )
                .into());
            }
            if requested.symlinkable && !perms.symlinkable {
                return Err(EvalError::user_error(
                    "narrow: source DirCap does not have Symlinkable permission".to_string(),
                    call_span.clone(),
                )
                .into());
            }
            if requested.posix_permissions && !perms.posix_permissions {
                return Err(EvalError::user_error(
                    "narrow: source DirCap does not have PosixPermissions permission".to_string(),
                    call_span.clone(),
                )
                .into());
            }
            if requested.extended_attributes && !perms.extended_attributes {
                return Err(EvalError::user_error(
                    "narrow: source DirCap does not have ExtendedAttributes permission".to_string(),
                    call_span.clone(),
                )
                .into());
            }

            ok_val(
                Value::DirCap {
                    dir: Rc::clone(dir),
                    perms: narrowed_perms,
                },
                call_span,
            )
        } else {
            // Invalid second argument
            Err(EvalError::type_mismatch_ctx(
                "narrow".to_string(),
                "String (subpath) or Variant (capability flag)",
                second_arg_val.type_name(),
                args[1].span.clone(),
            )
            .into())
        }
    })
}

/// `revocable`: Wrap a DirCap in a RevocableDirCap.
/// Takes a DirCap, returns a RevocableDirCap.
/// The RevocableDirCap can be revoked later via `revoke-cap`.
pub(crate) fn builtin_revocable(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = crate::builtins::expect_one_arg(
            "revocable",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

        // Extract DirCap and preserve permissions
        let (dir, perms) = match val {
            Value::DirCap { dir, perms } => (Rc::clone(&dir), perms.clone()),
            Value::RevocableDirCap {
                inner,
                perms,
                revoked: _,
            } => {
                // Already revocable — return a new revocable wrapper with a new flag
                // (allows independent revocation)
                (Rc::clone(&inner), perms.clone())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "revocable".to_string(),
                    "DirCap",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // Create a new revoked flag
        let revoked = Rc::new(std::cell::Cell::new(false));

        ok_val(
            Value::RevocableDirCap {
                inner: dir,
                perms,
                revoked,
            },
            call_span,
        )
    })
}

/// `revoke-cap`: Revoke a RevocableDirCap.
/// Takes a RevocableDirCap, sets its revoked flag to true, returns null.
/// Future operations on the cap will fail.
pub(crate) fn builtin_revoke_cap(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = crate::builtins::expect_one_arg(
            "revoke-cap",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

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
                args[0].span.clone(),
            )
            .into()),
        }
    })
}

/// `string-handle`: Wrap a String as a readable Handle backed by std::io::Cursor.
/// Takes a String and returns Handle[Readable] (text-mode, not Binary).
/// Compatible with builtin-read-line and builtin-read-chunk.
pub(crate) fn builtin_string_handle(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect exactly 1 arg: String
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        reject_named("string-handle", named.as_ref(), call_span.clone())?;

        // Get string value (pre-materialized by Strictness::Seq)
        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");

        let s = require_string("string-handle", val, args[0].span.clone())?;

        // Create Cursor as reader
        let cursor = std::io::Cursor::new(s.into_bytes());
        let handle: Box<dyn std::io::BufRead> = Box::new(cursor);

        // Create Handle with Readable + Text caps (matching stdin pattern from main.rs)
        let mut caps = HashMap::new();
        caps.insert("Readable".to_string(), Value::Bool(true));
        caps.insert("Text".to_string(), Value::Bool(true));

        ok_val(
            Value::Handle {
                caps,
                inner: Rc::new(std::cell::RefCell::new(handle)),
                write_inner: None,
                seek_inner: None,
                raw_tcp: None,
                creation_span: call_span.clone(),
            },
            call_span,
        )
    })
}

/// `builtin-read-line`: Read a single line from a Handle (Text mode).
/// Takes a Handle and returns String on success, [] (null) on EOF.
/// Strips trailing `\n` and `\r\n` from the result.
/// Rejects Binary-mode handles (error: "builtin-read-line requires a text-mode Handle, not Binary").
///
/// # EOF Behavior
///
/// When the handle is positioned at EOF, returns `Value::Dict(IndexMap::new())` — the tinct
/// null value (`[]`). Callers must distinguish EOF from an empty-line read: an empty line
/// returns `String("")`, while EOF returns `[]`. Use `[null? result]` or `[= result []]`
/// to test for EOF.
///
/// # Corpus Tests
///
/// - `tests/corpus/eval/builtins/read_line_file.llt-eval` — successive reads, newline stripping, EOF detection
/// - `tests/corpus/eval/builtins/read_line_eof.llt-eval` — empty handle returns [] immediately
pub(crate) fn builtin_read_line(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg(
            "builtin-read-line",
            &args,
            named.as_ref(),
            &ctx_arg.ctx,
            call_span.clone(),
        )?;

        // Extract Handle
        let (handle, caps) = match val {
            Value::Handle { inner, caps, .. } => (inner, caps),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-read-line".to_string(),
                    "Handle",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // Reject Binary cap handles
        if caps.contains_key("Binary") {
            return Err(EvalError::user_error(
                "builtin-read-line requires a text-mode Handle, not Binary".to_string(),
                call_span,
            )
            .into());
        }

        use std::io::BufRead;
        let mut line = String::new();

        // Save match result before the RefMut borrow is dropped (avoids lifetime error in async).
        let read_result = handle.borrow_mut().read_line(&mut line);
        match read_result {
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
                ok_val(string_val(&line), call_span)
            }
            Err(e) => Err(EvalError::user_error(
                format!("builtin-read-line: read failed: {}", e),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-read-chunk`: Read n bytes from a Handle (works with both Text and Binary).
/// Takes a Handle and Int (chunk size n), returns Bytes on success (partial reads OK), [] (null) on EOF.
/// Errors on non-positive n: "builtin-read-chunk: chunk size must be positive".
pub(crate) fn builtin_read_chunk(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        // Expect exactly 2 args: Handle, Int
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("builtin-read-chunk", named.as_ref(), call_span.clone())?;

        // First arg: Handle
        let handle_val = crate::eval::materialize(&args[0], Some(&call_span), &ctx).await?; // H1: force_count migration pending
        let handle = match handle_val {
            Value::Handle { inner, .. } => Rc::clone(&inner),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-read-chunk".to_string(),
                    "Handle",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // Second arg: Int (chunk size)
        let size_val = crate::eval::materialize(&args[1], Some(&call_span), &ctx).await?; // H1: force_count migration pending
        let n = match size_val {
            Value::Int(i) => i,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-read-chunk".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // Validate n > 0
        if n <= 0 {
            return Err(EvalError::user_error(
                "builtin-read-chunk: chunk size must be positive".to_string(),
                call_span,
            )
            .into());
        }

        use std::io::Read;
        let mut buffer = vec![0u8; n as usize];

        let read_result = handle.borrow_mut().read(&mut buffer);
        match read_result {
            Ok(0) => {
                // EOF — return null (empty dict)
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
            Ok(bytes_read) => {
                // Got data (partial read is OK)
                let result = buffer[..bytes_read].to_vec();
                let len = result.len();
                ok_val(
                    Value::Bytes {
                        source: Rc::from(result),
                        start: 0,
                        end: len,
                    },
                    call_span,
                )
            }
            Err(e) => Err(EvalError::user_error(
                format!("builtin-read-chunk: read failed: {}", e),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-read-all`: Read all bytes from a Handle to EOF, returning content as a String.
///
/// Takes a single Handle argument (text-mode or binary-mode) and reads until EOF using
/// `read_to_string`. Returns the complete content as a String value.
///
/// This is an internal primitive used by the include pipeline (`prelude`'s `include` function
/// reads file content via `builtin-read-all` directly). It is NOT exported from
/// `stdlib/prelude.llt`.
///
/// # Errors
///
/// - Type mismatch: argument is not a Handle
/// - Read error: underlying I/O failure during `read_to_string`
/// - Encoding error: binary Handle content is not valid UTF-8
///
/// Registered via `builtin!("builtin-read-all", ...)` in `core_builtins()` (builtins_core.rs).
/// T-736 (S-786) will wire prelude's include pipeline to call it.
pub(crate) fn builtin_read_all(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect exactly 1 positional arg: Handle (pre-materialized by Strictness::Seq).
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        reject_named("builtin-read-all", named.as_ref(), call_span.clone())?;

        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");

        // Extract the inner BufRead from a Handle.
        let handle = match val {
            Value::Handle { inner, .. } => inner,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-read-all".to_string(),
                    "Handle",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        use std::io::Read;
        let mut content = String::new();
        // read_to_string reads until EOF; errors on invalid UTF-8.
        let read_result = handle.borrow_mut().read_to_string(&mut content);

        match read_result {
            Ok(_) => ok_val(string_val(&content), call_span),
            Err(e) => Err(EvalError::user_error(
                format!("builtin-read-all: read failed: {}", e),
                call_span,
            )
            .into()),
        }
    })
}

/// `write`: Write a String to a file.
/// Takes a DirCap, String path, and String content.
/// Writes content to the file at path (creating or truncating), then returns empty dict `{}`.
pub(crate) fn builtin_write(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 3 args: DirCap, String path, String content
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }
        reject_named("write", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let content_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "write", args[0].span.clone())?;
        check_perm(
            perms,
            "Writable",
            perms.writable,
            "write",
            call_span.clone(),
        )?;

        let path = require_string("write", path_val, args[1].span.clone())?;
        let content = require_string("write", content_val, args[2].span.clone())?;

        // Open file for writing (create or truncate)
        use std::io::Write;
        let mut file = dir.create(&path).map_err(|e| {
            EvalError::user_error(
                format!("write: failed to create file '{}': {}", path, e),
                call_span.clone(),
            )
        })?;

        // Write content
        file.write_all(content.as_bytes()).map_err(|e| {
            EvalError::user_error(
                format!("write: failed to write to '{}': {}", path, e),
                call_span.clone(),
            )
        })?;

        // Return null (empty dict)
        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `write-atomic`: Atomically write a String to a file.
/// Takes a DirCap, String path, and String content.
/// Writes to a temp file in the same directory, then renames to the target path.
/// This ensures the target file is never partially written.
pub(crate) fn builtin_write_atomic(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 3 args: DirCap, String path, String content
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }
        reject_named("write-atomic", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let content_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "write-atomic", args[0].span.clone())?;
        check_perm(
            perms,
            "Writable",
            perms.writable,
            "write-atomic",
            call_span.clone(),
        )?;

        let path = require_string("write-atomic", path_val, args[1].span.clone())?;
        let content = require_string("write-atomic", content_val, args[2].span.clone())?;

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
                call_span.clone(),
            )
        })?;

        temp_file.write_all(content.as_bytes()).map_err(|e| {
            EvalError::user_error(
                format!(
                    "write-atomic: failed to write to temp file '{}': {}",
                    temp_name, e
                ),
                call_span.clone(),
            )
        })?;

        // Ensure data is flushed before rename
        temp_file.sync_all().map_err(|e| {
            EvalError::user_error(
                format!(
                    "write-atomic: failed to sync temp file '{}': {}",
                    temp_name, e
                ),
                call_span.clone(),
            )
        })?;

        // Drop the file handle before rename (required on Windows)
        drop(temp_file);

        // Atomically rename temp file to target path
        dir.rename(&temp_name, dir, &path).map_err(|e| {
            // Clean up temp file on rename failure
            let _ = dir.remove_file(&temp_name);
            EvalError::user_error(
                format!(
                    "write-atomic: failed to rename temp file to '{}': {}",
                    path, e
                ),
                call_span.clone(),
            )
        })?;

        // Return null (empty dict)
        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `cap-data`: Extract capability data from a Handle or WriteHandle.
/// Takes a Handle/WriteHandle and a capability name (String).
/// Returns the Value associated with that capability, or Null (empty dict) if the cap is absent.
pub(crate) fn builtin_cap_data(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 2 args: Handle/WriteHandle, String cap_name
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("cap-data", named.as_ref(), call_span.clone())?;

        let handle_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let cap_name_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract caps from Handle or WriteHandle
        let caps = match handle_val {
            Value::Handle { caps, .. } => caps,
            Value::WriteHandle { caps, .. } => caps,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "cap-data".to_string(),
                    "Handle or WriteHandle",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        let cap_name = require_string("cap-data", cap_name_val, args[1].span.clone())?;

        // Lookup capability — return empty dict (Null) on miss so callers can use null?
        match caps.get(&cap_name) {
            Some(cap_value) => ok_val(cap_value.clone(), call_span),
            None => ok_val(Value::Dict(IndexMap::new()), call_span),
        }
    })
}

/// `write-handle`: Write to a WriteHandle or a bidirectional Handle (e.g. TCP socket).
/// Takes a WriteHandle (or Handle with write_inner) and content (String for Text, Bytes for Binary).
/// Checks encoding via Binary cap: if present, content must be Bytes; otherwise String.
/// Uses `inner.borrow_mut().write_all(bytes)`.
/// Returns the original handle (WriteHandle or Handle) for chaining.
pub(crate) fn builtin_write_handle(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 2 args: WriteHandle or Handle, content
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("write-handle", named.as_ref(), call_span.clone())?;

        let handle_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let content_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

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
                    args[0].span.clone(),
                )
                .into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "write-handle".to_string(),
                    "WriteHandle or bidirectional Handle",
                    other.type_name(),
                    args[0].span.clone(),
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
                        args[1].span.clone(),
                    )
                    .into())
                }
            }
        } else {
            // Content must be String (Text encoding)
            let s = require_string("write-handle", content_val, args[1].span.clone())?;
            s.as_bytes().to_vec()
        };

        // Write to handle
        match &kind {
            HandleKind::Write { inner, .. } => {
                inner.borrow_mut().write_all(&bytes).map_err(|e| {
                    EvalError::user_error(
                        format!("write-handle: write failed: {}", e),
                        call_span.clone(),
                    )
                })?;
            }
            HandleKind::Bidirectional { write_inner, .. } => {
                write_inner.borrow_mut().write_all(&bytes).map_err(|e| {
                    EvalError::user_error(
                        format!("write-handle: write failed: {}", e),
                        call_span.clone(),
                    )
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
                    creation_span: call_span.clone(),
                },
                call_span,
            ),
        }
    })
}

/// `flush`: Flush a WriteHandle or bidirectional Handle buffer.
/// Takes a WriteHandle (or Handle with write_inner), flushes it, returns the same handle.
pub(crate) fn builtin_flush(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        let val = crate::builtins::expect_one_arg(
            "flush",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

        use std::io::Write;
        match val {
            Value::WriteHandle {
                ref inner,
                ref caps,
            } => {
                inner.borrow_mut().flush().map_err(|e| {
                    EvalError::user_error(format!("flush: flush failed: {}", e), call_span.clone())
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
                    EvalError::user_error(format!("flush: flush failed: {}", e), call_span.clone())
                })?;
                ok_val(
                    Value::Handle {
                        caps: caps.clone(),
                        inner: Rc::clone(inner),
                        write_inner: Some(Rc::clone(w)),
                        seek_inner: None,
                        raw_tcp: None,
                        creation_span: call_span.clone(),
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
                args[0].span.clone(),
            )
            .into()),
            other => Err(EvalError::type_mismatch_ctx(
                "flush".to_string(),
                "WriteHandle or bidirectional Handle",
                other.type_name(),
                args[0].span.clone(),
            )
            .into()),
        }
    })
}

/// `close`: Close a WriteHandle or bidirectional Handle.
/// Takes a WriteHandle (or Handle with write_inner), flushes and returns Null.
/// The inner writer is dropped when the last Rc is dropped.
pub(crate) fn builtin_close(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        let val = crate::builtins::expect_one_arg(
            "close",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

        use std::io::Write;
        match val {
            Value::WriteHandle { inner, .. } => {
                inner.borrow_mut().flush().map_err(|e| {
                    EvalError::user_error(format!("close: flush failed: {}", e), call_span.clone())
                })?;
            }
            Value::Handle {
                write_inner: Some(w),
                ..
            } => {
                w.borrow_mut().flush().map_err(|e| {
                    EvalError::user_error(format!("close: flush failed: {}", e), call_span.clone())
                })?;
            }
            Value::Handle {
                write_inner: None, ..
            } => {
                return Err(EvalError::type_mismatch_ctx(
                    "close".to_string(),
                    "WriteHandle or bidirectional Handle",
                    "read-only Handle",
                    args[0].span.clone(),
                )
                .into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "close".to_string(),
                    "WriteHandle or bidirectional Handle",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        }

        // Return Null (the inner writer is dropped when the Rc goes out of scope)
        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `raw-create`: Open a file for writing (create/truncate), returning a WriteHandle.
///
/// Takes 2 args: `cap` (DirCap), `path` (String).
/// Returns a WriteHandle with Writable and Text capabilities.
/// This is a low-level primitive used to implement higher-level I/O functions in tinct.
pub(crate) fn builtin_raw_create(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        reject_named("raw-create", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "raw-create", args[0].span.clone())?;

        // Check Writable permission
        check_perm(
            perms,
            "Writable",
            perms.writable,
            "raw-create",
            call_span.clone(),
        )?;

        let path = require_string("raw-create", path_val, args[1].span.clone())?;

        // Open file for writing (create/truncate)
        use cap_std::fs::OpenOptions;
        let file = dir
            .open_with(
                &path,
                OpenOptions::new().write(true).create(true).truncate(true),
            )
            .map_err(|e| {
                EvalError::user_error(
                    format!("raw-create: failed to create file '{}': {}", path, e),
                    call_span.clone(),
                )
            })?;

        // Create WriteHandle with Writable and Text capabilities
        let mut caps = HashMap::new();
        caps.insert("Writable".to_string(), Value::Bool(true));
        caps.insert("Text".to_string(), Value::Bool(true));

        use std::io::BufWriter;
        let writer: Box<dyn std::io::Write> = Box::new(BufWriter::new(file));

        ok_val(
            Value::WriteHandle {
                caps,
                inner: Rc::new(std::cell::RefCell::new(writer)),
            },
            call_span,
        )
    })
}

/// `seek`: Seek to a byte offset from the start of the file.
/// Takes a Handle and an Int offset, returns the Handle for chaining.
/// Requires the Seekable capability.
pub(crate) fn builtin_seek(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("seek", named.as_ref(), call_span.clone())?;

        let handle_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let offset_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract offset as Int
        let offset = match offset_val {
            Value::Int(i) => i,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "seek".to_string(),
                    "Int",
                    other.type_name(),
                    args[1].span.clone(),
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
                        args[0].span.clone(),
                    )
                    .into());
                }

                // Get the seek_inner
                let seek_handle = match seek_inner {
                    Some(s) => s,
                    None => {
                        return Err(EvalError::user_error(
                            "seek: Handle has Seekable capability but no seek interface"
                                .to_string(),
                            args[0].span.clone(),
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
                        EvalError::user_error(
                            format!("seek: seek failed: {}", e),
                            call_span.clone(),
                        )
                    })?;

                // Now seek the inner BufReader by downcasting
                // Since both are BufReader<cap_std::fs::File>, we can use std::any::Any
                use std::any::Any;
                let mut inner_borrow = inner.borrow_mut();
                if let Some(buf_reader) = (&mut *inner_borrow as &mut dyn Any)
                    .downcast_mut::<BufReader<cap_std::fs::File>>()
                {
                    buf_reader
                        .seek(std::io::SeekFrom::Start(offset as u64))
                        .map_err(|e| {
                            EvalError::user_error(
                                format!("seek: inner buffer seek failed: {}", e),
                                call_span.clone(),
                            )
                        })?;
                } else {
                    return Err(EvalError::user_error(
                        "seek: failed to downcast BufRead to BufReader<File>".to_string(),
                        call_span.clone(),
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
                        creation_span: call_span.clone(),
                    },
                    call_span,
                )
            }
            other => Err(EvalError::type_mismatch_ctx(
                "seek".to_string(),
                "Handle",
                other.type_name(),
                args[0].span.clone(),
            )
            .into()),
        }
    })
}

/// `seek-end`: Seek to the end of the file.
/// Takes a Handle, returns the Handle for chaining.
/// Requires the Seekable capability.
pub(crate) fn builtin_seek_end(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        let val = crate::builtins::expect_one_arg(
            "seek-end",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

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
                        args[0].span.clone(),
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
                            args[0].span.clone(),
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
                        EvalError::user_error(
                            format!("seek-end: seek failed: {}", e),
                            call_span.clone(),
                        )
                    })?;

                // Now seek the inner BufReader by downcasting
                use std::any::Any;
                let mut inner_borrow = inner.borrow_mut();
                if let Some(buf_reader) = (&mut *inner_borrow as &mut dyn Any)
                    .downcast_mut::<BufReader<cap_std::fs::File>>()
                {
                    buf_reader.seek(std::io::SeekFrom::End(0)).map_err(|e| {
                        EvalError::user_error(
                            format!("seek-end: inner buffer seek failed: {}", e),
                            call_span.clone(),
                        )
                    })?;
                } else {
                    return Err(EvalError::user_error(
                        "seek-end: failed to downcast BufRead to BufReader<File>".to_string(),
                        call_span.clone(),
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
                        creation_span: call_span.clone(),
                    },
                    call_span,
                )
            }
            other => Err(EvalError::type_mismatch_ctx(
                "seek-end".to_string(),
                "Handle",
                other.type_name(),
                args[0].span.clone(),
            )
            .into()),
        }
    })
}

/// `position`: Get the current byte offset in the file.
/// Takes a Handle, returns an Int.
/// Requires the Seekable capability.
pub(crate) fn builtin_position(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        let val = crate::builtins::expect_one_arg(
            "position",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

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
                        args[0].span.clone(),
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
                            args[0].span.clone(),
                        )
                        .into())
                    }
                };

                // Get the current position
                use std::io::Seek;
                let pos = seek_handle.borrow_mut().stream_position().map_err(|e| {
                    EvalError::user_error(
                        format!("position: failed to get position: {}", e),
                        call_span.clone(),
                    )
                })?;

                ok_val(Value::Int(pos as i64), call_span)
            }
            other => Err(EvalError::type_mismatch_ctx(
                "position".to_string(),
                "Handle",
                other.type_name(),
                args[0].span.clone(),
            )
            .into()),
        }
    })
}

/// `list-dir`: List directory entries with metadata.
/// Takes a DirCap and String path, returns a Seq of metadata Dicts.
/// Each dict has keys: name, type, size, mtime.
pub(crate) fn builtin_list_dir(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
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
        reject_named("list-dir", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "list-dir", args[0].span.clone())?;
        check_perm(
            perms,
            "Listable",
            perms.listable,
            "list-dir",
            call_span.clone(),
        )?;

        let path = require_string("list-dir", path_val, args[1].span.clone())?;

        // Read directory entries
        let entries = dir.read_dir(&path).map_err(|e| {
            EvalError::user_error(
                format!("list-dir: failed to read directory '{}': {}", path, e),
                call_span.clone(),
            )
        })?;

        // Collect entries into a vector
        let mut entry_values = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| {
                EvalError::user_error(
                    format!("list-dir: failed to read directory entry: {}", e),
                    call_span.clone(),
                )
            })?;

            let name = entry.file_name().to_string_lossy().to_string();
            let metadata = entry.metadata().map_err(|e| {
                EvalError::user_error(
                    format!("list-dir: failed to read metadata for '{}': {}", name, e),
                    call_span.clone(),
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
                Key::String("name".into()),
                ctx.alloc_thunk(ok_val(string_val(&name), call_span.clone())?),
            );
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(ok_val(string_val(file_type), call_span.clone())?),
            );
            dict.insert(
                Key::String("size".into()),
                ctx.alloc_thunk(ok_val(
                    Value::Int(metadata.len() as i64),
                    call_span.clone(),
                )?),
            );
            dict.insert(
                Key::String("mtime".into()),
                ctx.alloc_thunk(ok_val(Value::Int(mtime), call_span.clone())?),
            );

            entry_values.push(Value::Dict(dict));
        }

        // Build a sequence from the collected entries
        let mut seq = crate::value::make_seq_nil();
        for entry in entry_values.into_iter().rev() {
            let head_id = ctx.alloc_thunk(ok_val(entry, call_span.clone())?);
            let tail_id = ctx.alloc_thunk(ok_val(seq, call_span.clone())?);
            seq = crate::value::make_seq_cons(head_id, tail_id, &ctx);
        }

        ok_val(seq, call_span)
    })
}

/// `stat`: Get metadata for a file or directory.
/// Takes a DirCap and String path, returns a metadata Dict.
/// Dict has keys: name, type, size, mtime, mode, is-dir, is-file, is-symlink.
pub(crate) fn builtin_stat(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
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
        reject_named("stat", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "stat", args[0].span.clone())?;
        check_perm(perms, "Statable", perms.statable, "stat", call_span.clone())?;

        let path = require_string("stat", path_val, args[1].span.clone())?;

        // Get metadata
        let metadata = dir.metadata(&path).map_err(|e| {
            EvalError::user_error(
                format!("stat: failed to get metadata for '{}': {}", path, e),
                call_span.clone(),
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
            Key::String("name".into()),
            ctx.alloc_thunk(ok_val(string_val(&path), call_span.clone())?),
        );
        dict.insert(
            Key::String("type".into()),
            ctx.alloc_thunk(ok_val(string_val(file_type), call_span.clone())?),
        );
        dict.insert(
            Key::String("size".into()),
            ctx.alloc_thunk(ok_val(
                Value::Int(metadata.len() as i64),
                call_span.clone(),
            )?),
        );
        dict.insert(
            Key::String("mtime".into()),
            ctx.alloc_thunk(ok_val(Value::Int(mtime), call_span.clone())?),
        );
        dict.insert(
            Key::String("mode".into()),
            ctx.alloc_thunk(ok_val(Value::Int(mode), call_span.clone())?),
        );
        dict.insert(
            Key::String("is-dir".into()),
            ctx.alloc_thunk(ok_val(Value::Bool(metadata.is_dir()), call_span.clone())?),
        );
        dict.insert(
            Key::String("is-file".into()),
            ctx.alloc_thunk(ok_val(Value::Bool(metadata.is_file()), call_span.clone())?),
        );
        dict.insert(
            Key::String("is-symlink".into()),
            ctx.alloc_thunk(ok_val(
                Value::Bool(metadata.is_symlink()),
                call_span.clone(),
            )?),
        );

        ok_val(Value::Dict(dict), call_span)
    })
}

/// `exists`: Check if a path exists within a DirCap.
/// Returns Bool (true if exists, false if not).
/// Cheaper than try+stat for existence checks.
pub(crate) fn builtin_exists(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 2 args: DirCap, String path
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("exists", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "exists", args[0].span.clone())?;
        check_perm(
            perms,
            "Statable",
            perms.statable,
            "exists",
            call_span.clone(),
        )?;

        let path = require_string("exists", path_val, args[1].span.clone())?;

        // Check existence
        let exists = dir.try_exists(&path).map_err(|e| {
            EvalError::user_error(
                format!("exists: failed to check path '{}': {}", path, e),
                call_span.clone(),
            )
        })?;

        ok_val(Value::Bool(exists), call_span)
    })
}

/// `stat-symlink`: Get metadata for a path without following symlinks (lstat equivalent).
/// Returns a dict with the same schema as `stat`.
pub(crate) fn builtin_stat_symlink(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
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
        reject_named("stat-symlink", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "stat-symlink", args[0].span.clone())?;
        check_perm(
            perms,
            "Statable",
            perms.statable,
            "stat-symlink",
            call_span.clone(),
        )?;

        let path = require_string("stat-symlink", path_val, args[1].span.clone())?;

        // Get metadata without following symlinks
        let metadata = dir.symlink_metadata(&path).map_err(|e| {
            EvalError::user_error(
                format!("stat-symlink: failed to get metadata for '{}': {}", path, e),
                call_span.clone(),
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
            Key::String("name".into()),
            ctx.alloc_thunk(ok_val(string_val(&path), call_span.clone())?),
        );
        dict.insert(
            Key::String("type".into()),
            ctx.alloc_thunk(ok_val(string_val(file_type), call_span.clone())?),
        );
        dict.insert(
            Key::String("size".into()),
            ctx.alloc_thunk(ok_val(
                Value::Int(metadata.len() as i64),
                call_span.clone(),
            )?),
        );
        dict.insert(
            Key::String("mtime".into()),
            ctx.alloc_thunk(ok_val(Value::Int(mtime), call_span.clone())?),
        );
        dict.insert(
            Key::String("mode".into()),
            ctx.alloc_thunk(ok_val(Value::Int(mode), call_span.clone())?),
        );
        dict.insert(
            Key::String("is-dir".into()),
            ctx.alloc_thunk(ok_val(Value::Bool(metadata.is_dir()), call_span.clone())?),
        );
        dict.insert(
            Key::String("is-file".into()),
            ctx.alloc_thunk(ok_val(Value::Bool(metadata.is_file()), call_span.clone())?),
        );
        dict.insert(
            Key::String("is-symlink".into()),
            ctx.alloc_thunk(ok_val(
                Value::Bool(metadata.is_symlink()),
                call_span.clone(),
            )?),
        );

        ok_val(Value::Dict(dict), call_span)
    })
}

/// `copy-file`: Copy a file from one DirCap to another.
/// Takes 4 args: src DirCap, src path String, dst DirCap, dst path String.
/// Returns empty dict on success.
pub(crate) fn builtin_copy_file(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 4 args: src DirCap, src path, dst DirCap, dst path
        if args.len() != 4 {
            return Err(EvalError::arity_mismatch(4, args.len(), call_span).into());
        }
        reject_named("copy-file", named.as_ref(), call_span.clone())?;

        let src_dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let src_path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let dst_dir_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let dst_path_val = args[3]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract src DirCap and check permissions
        let (src_dir, src_perms) =
            extract_dir_cap(&src_dir_val, "copy-file", args[0].span.clone())?;
        check_perm(
            src_perms,
            "Readable",
            src_perms.readable,
            "copy-file",
            call_span.clone(),
        )?;

        // Extract dst DirCap and check permissions
        let (dst_dir, dst_perms) =
            extract_dir_cap(&dst_dir_val, "copy-file", args[2].span.clone())?;
        check_perm(
            dst_perms,
            "Writable",
            dst_perms.writable,
            "copy-file",
            call_span.clone(),
        )?;

        let src_path = require_string("copy-file", src_path_val, args[1].span.clone())?;
        let dst_path = require_string("copy-file", dst_path_val, args[3].span.clone())?;

        // Copy file using cap-std's efficient kernel-level copy
        src_dir.copy(&src_path, dst_dir, &dst_path).map_err(|e| {
            EvalError::user_error(
                format!(
                    "copy-file: failed to copy '{}' to '{}': {}",
                    src_path, dst_path, e
                ),
                call_span.clone(),
            )
        })?;

        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `symlink`: Create a symbolic link.
/// Takes 3 args: DirCap, target String, link path String.
/// Returns empty dict on success.
pub(crate) fn builtin_symlink(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 3 args: DirCap, target, link path
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }
        reject_named("symlink", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let target_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let link_path_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "symlink", args[0].span.clone())?;
        check_perm(
            perms,
            "Symlinkable",
            perms.symlinkable,
            "symlink",
            call_span.clone(),
        )?;

        let target = require_string("symlink", target_val, args[1].span.clone())?;
        let link_path = require_string("symlink", link_path_val, args[2].span.clone())?;

        // Create symlink (platform-specific)
        #[cfg(unix)]
        dir.symlink(&target, &link_path).map_err(|e| {
            EvalError::user_error(
                format!(
                    "symlink: failed to create symlink '{}' -> '{}': {}",
                    link_path, target, e
                ),
                call_span.clone(),
            )
        })?;

        #[cfg(windows)]
        {
            // On Windows, we need to know if the target is a file or directory
            // Try to stat the target to determine type
            let is_dir = match dir.metadata(&target) {
                Ok(m) => m.is_dir(),
                Err(e) => {
                    return Err(EvalError::user_error(
                        format!("symlink: cannot stat target '{}': {}", target, e),
                        call_span.clone(),
                    )
                    .into());
                }
            };
            if is_dir {
                dir.symlink_dir(&target, &link_path)
            } else {
                dir.symlink_file(&target, &link_path)
            }
            .map_err(|e| {
                EvalError::user_error(
                    format!(
                        "symlink: failed to create symlink '{}' -> '{}': {}",
                        link_path, target, e
                    ),
                    call_span.clone(),
                )
            })?;
        }

        #[cfg(not(any(unix, windows)))]
        {
            return Err(EvalError::user_error(
                "symlink: not supported on this platform".to_string(),
                call_span.clone(),
            )
            .into());
        }

        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `set-permissions`: Set file permissions (Unix mode).
/// Takes 3 args: DirCap, path String, mode Int (e.g., 0o755).
/// Returns empty dict on success.
/// Only works on Unix-like systems.
pub(crate) fn builtin_set_permissions(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 3 args: DirCap, path, mode
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }
        reject_named("set-permissions", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let mode_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "set-permissions", args[0].span.clone())?;
        check_perm(
            perms,
            "PosixPermissions",
            perms.posix_permissions,
            "set-permissions",
            call_span.clone(),
        )?;

        let path = require_string("set-permissions", path_val, args[1].span.clone())?;

        // Extract mode as Int
        let mode = match mode_val {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "set-permissions".to_string(),
                    "Int",
                    other.type_name(),
                    args[2].span.clone(),
                )
                .into());
            }
        };

        // Set permissions (Unix only)
        #[cfg(unix)]
        {
            use cap_std::fs::{Permissions, PermissionsExt};

            if !(0..=0o7777).contains(&mode) {
                return Err(EvalError::user_error(
                    format!("set-permissions: mode {} out of range (0-0o7777)", mode),
                    call_span.clone(),
                )
                .into());
            }

            let permissions = Permissions::from_mode(mode as u32);
            dir.set_permissions(&path, permissions).map_err(|e| {
                EvalError::user_error(
                    format!(
                        "set-permissions: failed to set permissions on '{}': {}",
                        path, e
                    ),
                    call_span.clone(),
                )
            })?;
        }

        #[cfg(not(unix))]
        {
            return Err(EvalError::user_error(
                "set-permissions: only supported on Unix-like systems".to_string(),
                call_span.clone(),
            )
            .into());
        }

        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `get-xattr`: Get an extended attribute value from a file (Linux only).
/// Takes a DirCap, path String, and attribute name String.
/// Returns Bytes value or [] if attribute not found.
/// Requires ExtendedAttributes permission.
#[cfg(target_os = "linux")]
pub(crate) fn builtin_get_xattr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 3 args: DirCap, path, attribute name
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }
        reject_named("get-xattr", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let name_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "get-xattr", args[0].span.clone())?;
        check_perm(
            perms,
            "ExtendedAttributes",
            perms.extended_attributes,
            "get-xattr",
            call_span.clone(),
        )?;

        let path = require_string("get-xattr", path_val, args[1].span.clone())?;
        let attr_name = require_string("get-xattr", name_val, args[2].span.clone())?;

        // Get the file via DirCap
        let file = dir.open(&path).map_err(|e| {
            EvalError::user_error(
                format!("get-xattr: failed to open file '{}': {}", path, e),
                call_span.clone(),
            )
        })?;

        // Get the extended attribute using the /proc/self/fd/{fd} path.
        // cap_std::fs::File does not implement xattr::FileExt, so we use the raw fd
        // via /proc/self/fd/ which preserves the capability-opened file semantics.
        use std::os::unix::io::AsRawFd;
        let raw_fd = file.as_raw_fd();
        let proc_path = format!("/proc/self/fd/{}", raw_fd);
        match xattr::get(&proc_path, &attr_name) {
            Ok(Some(value)) => {
                let len = value.len();
                ok_val(
                    Value::Bytes {
                        source: Rc::from(value.as_slice()),
                        start: 0,
                        end: len,
                    },
                    call_span,
                )
            }
            Ok(None) => {
                // Attribute not found — return []
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
            Err(e) => Err(EvalError::user_error(
                format!(
                    "get-xattr: failed to get attribute '{}' on '{}': {}",
                    attr_name, path, e
                ),
                call_span,
            )
            .into()),
        }
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn builtin_get_xattr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs { call_span, .. } = ctx_arg;
        Err(EvalError::user_error(
            "get-xattr: extended attributes are only supported on Linux".to_string(),
            call_span,
        )
        .into())
    })
}

/// `set-xattr`: Set an extended attribute on a file (Linux only).
/// Takes a DirCap, path String, attribute name String, and value Bytes.
/// Returns [] on success.
/// Requires ExtendedAttributes and Writable permissions.
#[cfg(target_os = "linux")]
pub(crate) fn builtin_set_xattr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 4 args: DirCap, path, attribute name, value
        if args.len() != 4 {
            return Err(EvalError::arity_mismatch(4, args.len(), call_span).into());
        }
        reject_named("set-xattr", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let name_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let value_val = args[3]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "set-xattr", args[0].span.clone())?;
        check_perm(
            perms,
            "ExtendedAttributes",
            perms.extended_attributes,
            "set-xattr",
            call_span.clone(),
        )?;
        check_perm(
            perms,
            "Writable",
            perms.writable,
            "set-xattr",
            call_span.clone(),
        )?;

        let path = require_string("set-xattr", path_val, args[1].span.clone())?;
        let attr_name = require_string("set-xattr", name_val, args[2].span.clone())?;

        // Extract value as Bytes
        let value_bytes = match value_val {
            Value::Bytes { source, start, end } => source[start..end].to_vec(),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "set-xattr".to_string(),
                    "Bytes",
                    other.type_name(),
                    args[3].span.clone(),
                )
                .into())
            }
        };

        // Get the file via DirCap
        let file = dir.open(&path).map_err(|e| {
            EvalError::user_error(
                format!("set-xattr: failed to open file '{}': {}", path, e),
                call_span.clone(),
            )
        })?;

        // Set the extended attribute using the /proc/self/fd/{fd} path.
        // cap_std::fs::File does not implement xattr::FileExt, so we use the raw fd
        // via /proc/self/fd/ which preserves the capability-opened file semantics.
        use std::os::unix::io::AsRawFd;
        let raw_fd = file.as_raw_fd();
        let proc_path = format!("/proc/self/fd/{}", raw_fd);
        xattr::set(&proc_path, &attr_name, &value_bytes).map_err(|e| {
            EvalError::user_error(
                format!(
                    "set-xattr: failed to set attribute '{}' on '{}': {}",
                    attr_name, path, e
                ),
                call_span.clone(),
            )
        })?;

        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn builtin_set_xattr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs { call_span, .. } = ctx_arg;
        Err(EvalError::user_error(
            "set-xattr: extended attributes are only supported on Linux".to_string(),
            call_span,
        )
        .into())
    })
}

/// `remove-xattr`: Remove an extended attribute from a file (Linux only).
/// Takes a DirCap, path String, and attribute name String.
/// Returns [] on success. No-ops gracefully if attribute doesn't exist.
/// Requires ExtendedAttributes and Writable permissions.
#[cfg(target_os = "linux")]
pub(crate) fn builtin_remove_xattr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 3 args: DirCap, path, attribute name
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }
        reject_named("remove-xattr", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let name_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "remove-xattr", args[0].span.clone())?;
        check_perm(
            perms,
            "ExtendedAttributes",
            perms.extended_attributes,
            "remove-xattr",
            call_span.clone(),
        )?;
        check_perm(
            perms,
            "Writable",
            perms.writable,
            "remove-xattr",
            call_span.clone(),
        )?;

        let path = require_string("remove-xattr", path_val, args[1].span.clone())?;
        let attr_name = require_string("remove-xattr", name_val, args[2].span.clone())?;

        // Get the file via DirCap
        let file = dir.open(&path).map_err(|e| {
            EvalError::user_error(
                format!("remove-xattr: failed to open file '{}': {}", path, e),
                call_span.clone(),
            )
        })?;

        // Remove the extended attribute using the /proc/self/fd/{fd} path.
        // cap_std::fs::File does not implement xattr::FileExt, so we use the raw fd
        // via /proc/self/fd/ which preserves the capability-opened file semantics.
        use std::os::unix::io::AsRawFd;
        let raw_fd = file.as_raw_fd();
        let proc_path = format!("/proc/self/fd/{}", raw_fd);
        match xattr::remove(&proc_path, &attr_name) {
            Ok(()) => ok_val(Value::Dict(IndexMap::new()), call_span),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // ENODATA — attribute doesn't exist, no-op gracefully
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
            Err(e) => Err(EvalError::user_error(
                format!(
                    "remove-xattr: failed to remove attribute '{}' on '{}': {}",
                    attr_name, path, e
                ),
                call_span,
            )
            .into()),
        }
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn builtin_remove_xattr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs { call_span, .. } = ctx_arg;
        Err(EvalError::user_error(
            "remove-xattr: extended attributes are only supported on Linux".to_string(),
            call_span,
        )
        .into())
    })
}

/// `list-xattrs`: List all extended attribute names on a file (Linux only).
/// Takes a DirCap and path String.
/// Returns a Seq of attribute name Strings.
/// Requires ExtendedAttributes permission.
#[cfg(target_os = "linux")]
pub(crate) fn builtin_list_xattrs(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        // Expect 2 args: DirCap, path
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("list-xattrs", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "list-xattrs", args[0].span.clone())?;
        check_perm(
            perms,
            "ExtendedAttributes",
            perms.extended_attributes,
            "list-xattrs",
            call_span.clone(),
        )?;

        let path = require_string("list-xattrs", path_val, args[1].span.clone())?;

        // Get the file via DirCap
        let file = dir.open(&path).map_err(|e| {
            EvalError::user_error(
                format!("list-xattrs: failed to open file '{}': {}", path, e),
                call_span.clone(),
            )
        })?;

        // List extended attributes using the /proc/self/fd/{fd} path.
        // cap_std::fs::File does not implement xattr::FileExt, so we use the raw fd
        // via /proc/self/fd/ which preserves the capability-opened file semantics.
        use std::os::unix::io::AsRawFd;
        let raw_fd = file.as_raw_fd();
        let proc_path = format!("/proc/self/fd/{}", raw_fd);
        let names = xattr::list(&proc_path).map_err(|e| {
            EvalError::user_error(
                format!(
                    "list-xattrs: failed to list attributes on '{}': {}",
                    path, e
                ),
                call_span.clone(),
            )
        })?;

        // Convert names to a Seq of Strings
        use crate::value::string_val;
        let name_values: Vec<_> = names
            .into_iter()
            .map(|name| {
                let name_str = name.to_string_lossy().to_string();
                string_val(&name_str)
            })
            .collect();

        // Build a Seq from the list
        let mut seq = crate::value::make_seq_nil();
        for name_val in name_values.into_iter().rev() {
            let head_id = ctx.alloc_thunk(ok_val(name_val, call_span.clone())?);
            let tail_id = ctx.alloc_thunk(ok_val(seq, call_span.clone())?);
            seq = crate::value::make_seq_cons(head_id, tail_id, &ctx);
        }

        ok_val(seq, call_span)
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn builtin_list_xattrs(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs { call_span, .. } = ctx_arg;
        Err(EvalError::user_error(
            "list-xattrs: extended attributes are only supported on Linux".to_string(),
            call_span,
        )
        .into())
    })
}

/// `make-dir`: Create a directory (and parent directories if needed).
/// Takes a DirCap and String path, returns Null.
pub(crate) fn builtin_make_dir(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 2 args: DirCap, String path
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("make-dir", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "make-dir", args[0].span.clone())?;
        check_perm(
            perms,
            "Writable",
            perms.writable,
            "make-dir",
            call_span.clone(),
        )?;

        let path = require_string("make-dir", path_val, args[1].span.clone())?;

        // Create directory (and parents)
        dir.create_dir_all(&path).map_err(|e| {
            EvalError::user_error(
                format!("make-dir: failed to create directory '{}': {}", path, e),
                call_span.clone(),
            )
        })?;

        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `remove`: Remove a file or empty directory.
/// Takes a DirCap and String path, returns Null.
/// Tries to remove as file first, then as directory.
pub(crate) fn builtin_remove(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 2 args: DirCap, String path
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("remove", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "remove", args[0].span.clone())?;
        check_perm(
            perms,
            "Deletable",
            perms.deletable,
            "remove",
            call_span.clone(),
        )?;

        let path = require_string("remove", path_val, args[1].span.clone())?;

        // Try to remove as file first, then as directory
        if let Err(file_err) = dir.remove_file(&path) {
            dir.remove_dir(&path).map_err(|dir_err| {
                EvalError::user_error(
                    format!(
                        "remove: failed to remove '{}' (as file: {}, as dir: {})",
                        path, file_err, dir_err
                    ),
                    call_span.clone(),
                )
            })?;
        }

        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `rename`: Rename or move a file or directory.
/// Takes a DirCap, old path String, and new path String, returns Null.
pub(crate) fn builtin_rename(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 3 args: DirCap, String old_path, String new_path
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }
        reject_named("rename", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let old_path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let new_path_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "rename", args[0].span.clone())?;
        check_perm(
            perms,
            "Renameable",
            perms.renameable,
            "rename",
            call_span.clone(),
        )?;

        let old_path = require_string("rename", old_path_val, args[1].span.clone())?;
        let new_path = require_string("rename", new_path_val, args[2].span.clone())?;

        // Rename (both source and dest are in the same DirCap)
        dir.rename(&old_path, dir, &new_path).map_err(|e| {
            EvalError::user_error(
                format!(
                    "rename: failed to rename '{}' to '{}': {}",
                    old_path, new_path, e
                ),
                call_span.clone(),
            )
        })?;

        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `copy`: Copy a file.
/// Takes a DirCap, source path String, and destination path String, returns Null.
/// `link`: Create a hard link.
/// Takes a DirCap, existing path String, and link path String, returns Null.
pub(crate) fn builtin_link(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 3 args: DirCap, String existing_path, String link_path
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }
        reject_named("link", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let existing_path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let link_path_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "link", args[0].span.clone())?;
        check_perm(perms, "Writable", perms.writable, "link", call_span.clone())?;

        let existing_path = require_string("link", existing_path_val, args[1].span.clone())?;
        let link_path = require_string("link", link_path_val, args[2].span.clone())?;

        // Create hard link
        dir.hard_link(&existing_path, dir, &link_path)
            .map_err(|e| {
                EvalError::user_error(
                    format!(
                        "link: failed to create hard link from '{}' to '{}': {}",
                        existing_path, link_path, e
                    ),
                    call_span.clone(),
                )
            })?;

        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `read-link`: Read the target of a symbolic link.
/// Takes a DirCap and String path, returns the target path as a String.
pub(crate) fn builtin_read_link(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        // Expect 2 args: DirCap, String path
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("read-link", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract DirCap and check permissions
        let (dir, perms) = extract_dir_cap(&dir_val, "read-link", args[0].span.clone())?;
        check_perm(
            perms,
            "Readable",
            perms.readable,
            "read-link",
            call_span.clone(),
        )?;

        let path = require_string("read-link", path_val, args[1].span.clone())?;

        // Read symlink target
        let target = dir.read_link(&path).map_err(|e| {
            EvalError::user_error(
                format!("read-link: failed to read symlink '{}': {}", path, e),
                call_span.clone(),
            )
        })?;

        let target_str = target.to_string_lossy().to_string();
        ok_val(string_val(&target_str), call_span)
    })
}

/// Register `builtin-*` type aliases for I/O builtins (T-1102).
///
/// Each alias copies the TypeScheme from the canonical name already registered in
/// `core_type_env`. Call this AFTER `core_type_env` has run.
pub fn io_builtin_types(env: &mut crate::types::TypeEnv) {
    env.alias_types(&[
        ("builtin-env", "env"),
        ("builtin-emit", "emit"),
        ("builtin-open", "open"),
        ("builtin-write", "write"),
        ("builtin-write-atomic", "write-atomic"),
        ("builtin-write-handle", "write-handle"),
        ("builtin-flush", "flush"),
        ("builtin-close", "close"),
        ("builtin-stat", "stat"),
        ("builtin-exists", "exists"),
        ("builtin-stat-symlink", "stat-symlink"),
        ("builtin-make-dir", "make-dir"),
        ("builtin-rename", "rename"),
        ("builtin-list-dir", "list-dir"),
        ("builtin-string-handle", "string-handle"),
        ("builtin-copy-file", "copy-file"),
        ("builtin-symlink", "symlink"),
        ("builtin-set-permissions", "set-permissions"),
        ("builtin-link", "link"),
        ("builtin-read-link", "read-link"),
        ("builtin-get-xattr", "get-xattr"),
        ("builtin-set-xattr", "set-xattr"),
        ("builtin-remove-xattr", "remove-xattr"),
        ("builtin-list-xattrs", "list-xattrs"),
        ("builtin-raw-create", "raw-create"),
        ("builtin-seek", "seek"),
        ("builtin-seek-end", "seek-end"),
        ("builtin-position", "position"),
        ("builtin-revocable", "revocable"),
        ("builtin-revoke-cap", "revoke-cap"),
        ("builtin-cap-data", "cap-data"),
        ("builtin-connect", "connect"),
        ("builtin-tls-layer", "tls-layer"),
        ("builtin-tls-peer-cert", "tls-peer-cert"),
        ("builtin-send-datagram", "send-datagram"),
        ("builtin-recv-datagram", "recv-datagram"),
    ]);
}
