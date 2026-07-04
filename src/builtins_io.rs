//! Filesystem I/O builtins: file primitives, DirCap operations, and stateless stdio.
//!
//! **Design principle**: Rust exposes raw OS primitives as thinly as possible. tinct builds
//! abstractions. No buffering in Rust, no protocol in Rust. The `open` function in prelude.llt
//! wraps `builtin-file-open` in a protocol dict. `%stdout`/`%stderr` are protocol dicts in
//! loader.llt Dict 2 that call `builtin-write-stdout`/`builtin-write-stderr`.
//!
//! **File primitives (Value::File — thin OS wrappers, no buffering):**
//! - `builtin-file-open cap path mode` → `Value::File` (opens via cap_std::fs::Dir)
//! - `builtin-file-read f n` → `Value::Bytes` (reads up to n bytes, empty on EOF)
//! - `builtin-file-write f s` → `Value::Dict([])` (writes string bytes, no buffering)
//! - `builtin-file-flush f` → `Value::Dict([])` (flush)
//! - `builtin-file-close f` → `Value::Dict([])` (drops file)
//! - `builtin-file-seek f pos` → `Value::Dict([])` (seek from start)
//!
//! **Stateless stdio builtins:**
//! - `builtin-write-stdout s` → writes string to stdout
//! - `builtin-write-stderr s` → writes string to stderr
//! - `builtin-read-stdin n` → reads n bytes from stdin, returns `Value::Bytes`
//!
//! **DirCap operations:**
//! - `write`: Write a string to a file (DirCap-based, atomic via create/write)
//! - `write-atomic`: Atomically write to a file (temp + rename)
//! - `narrow`: Attenuate a DirCap to a subdirectory
//! - `revocable`: Wrap a DirCap in a revocable wrapper
//! - `revoke-cap`: Revoke a RevocableDirCap
//!
//! **Other:**
//! - `emit`: Write to stdout
//! - `env`: Read environment variables
//!
//! Network builtins are in `builtins_net.rs`. Handle/WriteHandle variants removed.
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration is via `core_builtins()` in `src/builtins_core.rs`, dispatched by
//! `builtin_module("core")` in `src/builtins.rs`.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::{ok_val, reject_named, require_string};
use crate::error::{EvalError, EvalResult};
use crate::value::{string_val, BuiltinArgs, DirPerms, HashableValue, Thunk, ThunkId, Value};

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
        ..
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

/// `builtin-env`: Read an environment variable by name.
///
/// Returns the value as a String. Raises a user error if the variable is not set
/// or if access to it is not allowed by `ctx.env_allowed`. Prelude wraps this with
/// `builtin-env-has?` to provide the Absent.Absent fallback for user-facing `env`.
///
/// Gated by ctx.env_allowed: None = unrestricted, Some(set) = only those in the set.
pub(crate) fn builtin_env(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
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
            return Err(EvalError::user_error(
                format!("environment variable not allowed: {name}"),
                call_span,
            )
            .into());
        }

        // Read env var
        match std::env::var(&name) {
            Ok(value) => ok_val(string_val(&value), call_span),
            Err(_) => Err(EvalError::user_error(
                format!("environment variable not set: {name}"),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-env-has?`: Check whether an environment variable exists and is allowed.
///
/// Returns `Int(1)` if the variable is set and access is permitted, `Int(0)` otherwise.
/// Prelude uses this together with `builtin-env` to implement the user-facing `env`
/// function that returns `Absent.Absent` for missing/disallowed variables without Rust
/// ever constructing the Absent sentinel directly.
///
/// Gated by ctx.env_allowed: None = unrestricted, Some(set) = only those in the set.
pub(crate) fn builtin_env_has(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let val = crate::builtins::expect_one_arg(
            "env-has?",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let name = require_string("env-has?", val, args[0].span.clone())?;

        // Check env_allowed
        let allowed = match &ctx.env_allowed {
            None => true,
            Some(set) => set.contains(&name),
        };

        if !allowed {
            return ok_val(Value::Int(0), call_span);
        }

        // Check whether the env var is set
        let present = std::env::var(&name).is_ok();
        ok_val(Value::Int(if present { 1 } else { 0 }), call_span)
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
        ..
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
        ..
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
        ..
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

// builtin_string_handle, builtin_read_line, builtin_read_chunk, builtin_read_all removed.
// These operated on Value::Handle which is gone. Network streams (builtins_net.rs) are
// redesigned separately. File reading uses builtin_file_read (Value::File).
// The builtin-read-all registration in builtins_core.rs also removed.

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
            ..
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
            ..
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

// builtin_cap_data, builtin_write_handle, builtin_flush, builtin_close,
// builtin_raw_create, builtin_seek, builtin_seek_end, builtin_position removed.
// These all operated on Value::Handle / Value::WriteHandle which are gone.
// Network I/O redesign is a separate effort in builtins_net.rs.

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
            ..
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
            let mut dict = IndexMap::new();
            dict.insert(
                HashableValue::Str("name".into()),
                ctx.alloc_thunk(ok_val(string_val(&name), call_span.clone())?),
            );
            dict.insert(
                HashableValue::Str("type".into()),
                ctx.alloc_thunk(ok_val(string_val(file_type), call_span.clone())?),
            );
            dict.insert(
                HashableValue::Str("size".into()),
                ctx.alloc_thunk(ok_val(
                    Value::Int(metadata.len() as i64),
                    call_span.clone(),
                )?),
            );
            dict.insert(
                HashableValue::Str("mtime".into()),
                ctx.alloc_thunk(ok_val(Value::Int(mtime), call_span.clone())?),
            );

            entry_values.push(Value::Dict(dict));
        }

        // Build an integer-keyed Dict from the collected entries
        let mut result: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        for (i, entry) in entry_values.into_iter().enumerate() {
            let id = ctx.alloc_thunk(ok_val(entry, call_span.clone())?);
            result.insert(HashableValue::Int(i as i64), id);
        }
        ok_val(Value::Dict(result), call_span)
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
            ..
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
        let mut dict = IndexMap::new();
        dict.insert(
            HashableValue::Str("name".into()),
            ctx.alloc_thunk(ok_val(string_val(&path), call_span.clone())?),
        );
        dict.insert(
            HashableValue::Str("type".into()),
            ctx.alloc_thunk(ok_val(string_val(file_type), call_span.clone())?),
        );
        dict.insert(
            HashableValue::Str("size".into()),
            ctx.alloc_thunk(ok_val(
                Value::Int(metadata.len() as i64),
                call_span.clone(),
            )?),
        );
        dict.insert(
            HashableValue::Str("mtime".into()),
            ctx.alloc_thunk(ok_val(Value::Int(mtime), call_span.clone())?),
        );
        dict.insert(
            HashableValue::Str("mode".into()),
            ctx.alloc_thunk(ok_val(Value::Int(mode), call_span.clone())?),
        );
        dict.insert(
            HashableValue::Str("is-dir".into()),
            ctx.alloc_thunk(ok_val(
                Value::Int(if metadata.is_dir() { 1 } else { 0 }),
                call_span.clone(),
            )?),
        );
        dict.insert(
            HashableValue::Str("is-file".into()),
            ctx.alloc_thunk(ok_val(
                Value::Int(if metadata.is_file() { 1 } else { 0 }),
                call_span.clone(),
            )?),
        );
        dict.insert(
            HashableValue::Str("is-symlink".into()),
            ctx.alloc_thunk(ok_val(
                Value::Int(if metadata.is_symlink() { 1 } else { 0 }),
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
            ..
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

        ok_val(Value::Int(if exists { 1 } else { 0 }), call_span)
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
            ..
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
        let mut dict = IndexMap::new();
        dict.insert(
            HashableValue::Str("name".into()),
            ctx.alloc_thunk(ok_val(string_val(&path), call_span.clone())?),
        );
        dict.insert(
            HashableValue::Str("type".into()),
            ctx.alloc_thunk(ok_val(string_val(file_type), call_span.clone())?),
        );
        dict.insert(
            HashableValue::Str("size".into()),
            ctx.alloc_thunk(ok_val(
                Value::Int(metadata.len() as i64),
                call_span.clone(),
            )?),
        );
        dict.insert(
            HashableValue::Str("mtime".into()),
            ctx.alloc_thunk(ok_val(Value::Int(mtime), call_span.clone())?),
        );
        dict.insert(
            HashableValue::Str("mode".into()),
            ctx.alloc_thunk(ok_val(Value::Int(mode), call_span.clone())?),
        );
        dict.insert(
            HashableValue::Str("is-dir".into()),
            ctx.alloc_thunk(ok_val(
                Value::Int(if metadata.is_dir() { 1 } else { 0 }),
                call_span.clone(),
            )?),
        );
        dict.insert(
            HashableValue::Str("is-file".into()),
            ctx.alloc_thunk(ok_val(
                Value::Int(if metadata.is_file() { 1 } else { 0 }),
                call_span.clone(),
            )?),
        );
        dict.insert(
            HashableValue::Str("is-symlink".into()),
            ctx.alloc_thunk(ok_val(
                Value::Int(if metadata.is_symlink() { 1 } else { 0 }),
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
            ..
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
            ..
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
            ..
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
            ..
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
            ..
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
            ..
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
            ..
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

        // Convert names to an integer-keyed Dict of Strings
        use crate::value::string_val;
        let mut result: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        for (i, name) in names.into_iter().enumerate() {
            let name_str = name.to_string_lossy().to_string();
            let id = ctx.alloc_thunk(ok_val(string_val(&name_str), call_span.clone())?);
            result.insert(HashableValue::Int(i as i64), id);
        }

        ok_val(Value::Dict(result), call_span)
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
            ..
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
            ..
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
            ..
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
            ..
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
            ..
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

/// `builtin-path-dir`: Given a String path (file path), return a DirCap for its parent directory.
///
/// The path is interpreted relative to `ctx.config.base_dir` (the sandbox root). The parent
/// directory component is extracted from the path and opened using cap_std's confined
/// directory API (RESOLVE_BENEATH: no escaping the sandbox).
///
/// Takes 1 arg: String (file path). Returns DirCap with full permissions of the parent.
///
/// # Example
///
/// ```llt
/// [builtin-path-dir "data/config/settings.json"]
/// # → DirCap for "data/config/" (relative to base dir)
/// ```
pub(crate) fn builtin_path_dir(
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
            "builtin-path-dir",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

        let path_str = require_string("builtin-path-dir", val, args[0].span.clone())?;

        // Extract parent directory component from the path string.
        let path = std::path::Path::new(&path_str);
        let parent = path.parent().unwrap_or(std::path::Path::new("."));

        // Open the parent directory within the cap_std sandbox (RESOLVE_BENEATH).
        // This prevents escape from ctx.config.base_dir regardless of the path content.
        let parent_str = parent.to_string_lossy();
        let opened = ctx
            .config
            .base_dir
            .open_dir(parent_str.as_ref())
            .map_err(|e| {
                EvalError::user_error(
                    format!(
                        "builtin-path-dir: failed to open parent directory '{}' of path '{}': {}",
                        parent_str, path_str, e
                    ),
                    call_span.clone(),
                )
            })?;

        ok_val(
            Value::DirCap {
                dir: Rc::new(opened),
                perms: DirPerms::full(),
            },
            call_span,
        )
    })
}

/// `builtin-file-open`: Open a file via a DirCap and return a raw `Value::File`.
///
/// Signature: `[builtin-file-open cap path mode]`
///
/// `mode` is a String:
/// - `"read"` → open for reading
/// - `"write"` → open for writing (truncate + create)
/// - `"append"` → open in append mode (create if absent)
/// - `"create"` → create file if it doesn't exist (write mode implied)
/// - `"truncate"` → truncate to zero length (write mode implied)
///
/// Returns `Value::File(Rc<RefCell<cap_std::fs::File>>)`.
/// All I/O protocol is built in tinct — `open` in prelude wraps this in a protocol dict.
pub(crate) fn builtin_file_open(
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

        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }
        reject_named("builtin-file-open", named.as_ref(), call_span.clone())?;

        let dir_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");
        let path_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[1]=Seq");
        let mode_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[2]=Seq");

        let (dir, _perms) = extract_dir_cap(&dir_val, "builtin-file-open", args[0].span.clone())?;
        let path = require_string("builtin-file-open", path_val, args[1].span.clone())?;
        let mode = require_string("builtin-file-open", mode_val, args[2].span.clone())?;

        use cap_std::fs::OpenOptions;

        let file = match mode.as_str() {
            "read" => dir.open(&path).map_err(|e| {
                EvalError::user_error(
                    format!("builtin-file-open: failed to open '{}' for reading: {}", path, e),
                    call_span.clone(),
                )
            })?,
            "write" => dir
                .open_with(
                    &path,
                    OpenOptions::new().write(true).create(true).truncate(true),
                )
                .map_err(|e| {
                    EvalError::user_error(
                        format!("builtin-file-open: failed to open '{}' for writing: {}", path, e),
                        call_span.clone(),
                    )
                })?,
            "append" => dir
                .open_with(&path, OpenOptions::new().append(true).create(true))
                .map_err(|e| {
                    EvalError::user_error(
                        format!(
                            "builtin-file-open: failed to open '{}' for appending: {}",
                            path, e
                        ),
                        call_span.clone(),
                    )
                })?,
            "create" => dir
                .open_with(
                    &path,
                    OpenOptions::new().write(true).create(true).truncate(true),
                )
                .map_err(|e| {
                    EvalError::user_error(
                        format!("builtin-file-open: failed to create '{}': {}", path, e),
                        call_span.clone(),
                    )
                })?,
            "truncate" => dir
                .open_with(
                    &path,
                    OpenOptions::new().write(true).create(true).truncate(true),
                )
                .map_err(|e| {
                    EvalError::user_error(
                        format!("builtin-file-open: failed to truncate '{}': {}", path, e),
                        call_span.clone(),
                    )
                })?,
            other => {
                return Err(EvalError::user_error(
                    format!(
                        "builtin-file-open: unknown mode \"{}\" (expected \"read\", \"write\", \"append\", \"create\", or \"truncate\")",
                        other
                    ),
                    call_span,
                )
                .into())
            }
        };

        ok_val(
            Value::File(Rc::new(std::cell::RefCell::new(file))),
            call_span,
        )
    })
}

/// `builtin-file-read`: Read up to n bytes from a `Value::File`.
///
/// Calls `std::io::Read::read()` directly — no buffering.
/// Returns `Value::Bytes` with the bytes read, or empty `Value::Bytes` on EOF.
pub(crate) fn builtin_file_read(
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
        reject_named("builtin-file-read", named.as_ref(), call_span.clone())?;

        let file_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");
        let n_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[1]=Seq");

        let file_rc = match file_val {
            Value::File(rc) => rc,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-file-read".to_string(),
                    "File",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        let n = match n_val {
            Value::Int(i) if i > 0 => i as usize,
            Value::Int(_) => {
                return Err(EvalError::user_error(
                    "builtin-file-read: byte count must be positive".to_string(),
                    call_span,
                )
                .into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-file-read".to_string(),
                    "Int",
                    other.type_name(),
                    args[1].span.clone(),
                )
                .into())
            }
        };

        use std::io::Read;
        let mut buf = vec![0u8; n];
        let bytes_read = file_rc.borrow_mut().read(&mut buf).map_err(|e| {
            EvalError::user_error(
                format!("builtin-file-read: read failed: {}", e),
                call_span.clone(),
            )
        })?;
        buf.truncate(bytes_read);
        let len = buf.len();
        ok_val(
            Value::Bytes {
                source: Rc::from(buf),
                start: 0,
                end: len,
            },
            call_span,
        )
    })
}

/// `builtin-file-write`: Write a String to a `Value::File`.
///
/// Calls `std::io::Write::write_all()` directly — no buffering.
/// Returns empty dict `[]` (null).
pub(crate) fn builtin_file_write(
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
        reject_named("builtin-file-write", named.as_ref(), call_span.clone())?;

        let file_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");
        let s_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[1]=Seq");

        let file_rc = match file_val {
            Value::File(rc) => rc,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-file-write".to_string(),
                    "File",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        let s = require_string("builtin-file-write", s_val, args[1].span.clone())?;

        use std::io::Write;
        file_rc.borrow_mut().write_all(s.as_bytes()).map_err(|e| {
            EvalError::user_error(
                format!("builtin-file-write: write failed: {}", e),
                call_span.clone(),
            )
        })?;

        Ok(Arc::clone(&args[0]))
    })
}

/// `builtin-file-flush`: Flush a `Value::File`.
///
/// Calls `std::io::Write::flush()`. Returns empty dict `[]`.
pub(crate) fn builtin_file_flush(
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
            "builtin-file-flush",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

        let file_rc = match val {
            Value::File(rc) => rc,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-file-flush".to_string(),
                    "File",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        use std::io::Write;
        file_rc.borrow_mut().flush().map_err(|e| {
            EvalError::user_error(
                format!("builtin-file-flush: flush failed: {}", e),
                call_span.clone(),
            )
        })?;

        ok_val(Value::Int(1), call_span)
    })
}

/// `builtin-file-close`: Close a `Value::File` by dropping it.
///
/// Drops the file reference. If no other clones exist, the OS file descriptor is closed.
/// Returns empty dict `[]`.
pub(crate) fn builtin_file_close(
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
            "builtin-file-close",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

        match val {
            Value::File(_) => {
                // Dropping the Rc here closes the file if no other references exist.
                drop(val);
                ok_val(Value::Int(1), call_span)
            }
            other => Err(EvalError::type_mismatch_ctx(
                "builtin-file-close".to_string(),
                "File",
                other.type_name(),
                args[0].span.clone(),
            )
            .into()),
        }
    })
}

/// `builtin-file-seek`: Seek to a byte position from the start of a `Value::File`.
///
/// Calls `std::io::Seek::seek(SeekFrom::Start(pos))`.
/// Returns empty dict `[]`.
pub(crate) fn builtin_file_seek(
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
        reject_named("builtin-file-seek", named.as_ref(), call_span.clone())?;

        let file_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");
        let pos_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[1]=Seq");

        let file_rc = match file_val {
            Value::File(rc) => rc,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-file-seek".to_string(),
                    "File",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        let pos = match pos_val {
            Value::Int(i) if i >= 0 => i as u64,
            Value::Int(_) => {
                return Err(EvalError::user_error(
                    "builtin-file-seek: position must be non-negative".to_string(),
                    call_span,
                )
                .into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-file-seek".to_string(),
                    "Int",
                    other.type_name(),
                    args[1].span.clone(),
                )
                .into())
            }
        };

        use std::io::Seek;
        file_rc
            .borrow_mut()
            .seek(std::io::SeekFrom::Start(pos))
            .map_err(|e| {
                EvalError::user_error(
                    format!("builtin-file-seek: seek failed: {}", e),
                    call_span.clone(),
                )
            })?;

        ok_val(Value::Int(1), call_span)
    })
}

/// `builtin-write-stdout`: Write a String to `std::io::stdout()`, return handle.
///
/// Args: (s: String, h: any). Writes s to stdout, returns h (the handle) for chaining.
/// h is passed through lazily — it is not materialized.
///
/// Used by the `%stdout` protocol dict in loader.llt Dict 2:
///   `write: [fn [let s h] [builtin-write-stdout s h]]`
pub(crate) fn builtin_write_stdout(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ..
        } = ctx_arg;

        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("builtin-write-stdout", named.as_ref(), call_span.clone())?;

        let s_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");

        let s = require_string("builtin-write-stdout", s_val, args[0].span.clone())?;

        use std::io::Write;
        std::io::stdout().write_all(s.as_bytes()).map_err(|e| {
            EvalError::user_error(format!("builtin-write-stdout: {e}"), call_span.clone())
        })?;

        Ok(Arc::clone(&args[1]))
    })
}

/// `builtin-write-stderr`: Write a String to `std::io::stderr()`, return handle.
///
/// Args: (s: String, h: any). Writes s to stderr, returns h (the handle) for chaining.
/// h is passed through lazily — it is not materialized.
pub(crate) fn builtin_write_stderr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ..
        } = ctx_arg;

        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("builtin-write-stderr", named.as_ref(), call_span.clone())?;

        let s_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");

        let s = require_string("builtin-write-stderr", s_val, args[0].span.clone())?;

        use std::io::Write;
        std::io::stderr().write_all(s.as_bytes()).map_err(|e| {
            EvalError::user_error(
                format!("builtin-write-stderr: write failed: {e}"),
                call_span.clone(),
            )
        })?;

        Ok(Arc::clone(&args[1]))
    })
}

/// `builtin-read-stdin`: Read up to n bytes from `std::io::stdin()`.
///
/// Calls `std::io::Read::read()` directly — no buffering.
/// Returns `Value::Bytes` (empty on EOF).
pub(crate) fn builtin_read_stdin(
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

        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        reject_named("builtin-read-stdin", named.as_ref(), call_span.clone())?;

        let n_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");

        let n = match n_val {
            Value::Int(i) if i > 0 => i as usize,
            Value::Int(_) => {
                return Err(EvalError::user_error(
                    "builtin-read-stdin: byte count must be positive".to_string(),
                    call_span,
                )
                .into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-read-stdin".to_string(),
                    "Int",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        use std::io::Read;
        let mut buf = vec![0u8; n];
        let bytes_read = std::io::stdin().read(&mut buf).map_err(|e| {
            EvalError::user_error(
                format!("builtin-read-stdin: read failed: {}", e),
                call_span.clone(),
            )
        })?;
        buf.truncate(bytes_read);
        let len = buf.len();
        ok_val(
            Value::Bytes {
                source: Rc::from(buf),
                start: 0,
                end: len,
            },
            call_span,
        )
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
        ("builtin-file-open", "open"),
        ("builtin-file-read", "read-chunk"),
        ("builtin-file-write", "write-handle"),
        ("builtin-file-flush", "flush"),
        ("builtin-file-close", "close"),
        ("builtin-file-seek", "seek"),
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
