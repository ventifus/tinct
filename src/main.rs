//! LLT command-line tool: parses and evaluates `.llt` files, outputs JSON or LLT display format.

use clap::{Parser, Subcommand, ValueEnum};
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::process;
use std::rc::Rc;
use tinct::{
    create_stdlib_env, deep_materialize, eval_file_with_input, format_source, format_with_json_llt,
    json_to_value, literate, materialize, parse, value_to_display_string, value_to_json,
    EvalContext, Span, Thunk, MAX_COLLECT_SIZE, MAX_FILE_SIZE,
};

// Exit codes for llt eval
const EXIT_ERROR: i32 = 1;
const EXIT_TIMEOUT: i32 = 2;
// Note: RLIMIT_AS violations cause SIGSEGV/SIGKILL from the kernel, not a clean exit code.
// RLIMIT_CPU violations cause SIGXCPU (soft) or SIGKILL (hard). Both terminate without EXIT_ERROR.

/// tinct -- a unified data representation and transformation language.
#[derive(Parser)]
#[command(name = "tinct", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Evaluate an LLT file and output the result.
    Eval {
        /// Output format.
        #[arg(short, long, default_value = "json", value_enum)]
        format: OutputFormat,

        /// Deep-force all thunks before serializing (surfaces errors before partial output).
        #[arg(long)]
        eval: bool,

        /// Disable filesystem access ($include).
        #[arg(long)]
        no_fs: bool,

        /// Require all $include calls to provide an integrity hash. Hashless includes error.
        #[arg(long)]
        require_integrity: bool,

        /// Wall-clock timeout (e.g. "5s", "500ms", "2m"). Exit code 2 on expiry.
        #[arg(long)]
        timeout: Option<String>,

        /// Restrict $include to files under the given directory (may be repeated).
        /// When any --allow-path flag is present, $include may only access paths
        /// that are descendants of at least one allowed root. Paths are canonicalized
        /// at startup. Use `.` to allow the current working directory.
        #[arg(long, value_name = "PATH")]
        allow_path: Vec<PathBuf>,

        /// Disable Landlock filesystem ACL enforcement even when --allow-path is set.
        /// By default, when --allow-path is specified on Linux, Landlock is applied as
        /// defense-in-depth. This flag skips that step (e.g., for older kernels or
        /// environments where Landlock is not available).
        #[arg(long)]
        no_landlock: bool,

        /// Maximum virtual address space (bytes) the process may use. Enforced via
        /// RLIMIT_AS. Default: 512 MB. Set to 0 to disable. (Unix only)
        #[arg(long, value_name = "BYTES")]
        max_memory: Option<u64>,

        /// Maximum CPU time (seconds) the process may consume. Enforced via
        /// RLIMIT_CPU. Sends SIGXCPU on soft limit, SIGKILL on hard limit.
        /// Complements --timeout (wall-clock). (Unix only)
        #[arg(long, value_name = "SECONDS")]
        max_cpu: Option<u64>,

        /// Maximum number of open file descriptors. Enforced via RLIMIT_NOFILE.
        /// Default: 64. Set to 0 to disable. (Unix only)
        #[arg(long, value_name = "COUNT")]
        max_fds: Option<u64>,

        /// Disable environment variable access. $env returns Null for all names.
        #[arg(long)]
        no_env: bool,

        /// Allow $env to read specific environment variable(s) by name (may be repeated).
        /// When any --allow-env flag is present, $env returns Null for unlisted names.
        #[arg(long, value_name = "NAME")]
        allow_env: Vec<String>,

        /// Do not inject `pwd` DirCap into the root environment.
        /// When set, [open pwd ...] and [include pwd ...] fail with undefined variable.
        #[arg(long)]
        no_pwd: bool,

        /// Do not inject `stdin` Handle into the root environment.
        /// When set, [slurp stdin] fails with undefined variable.
        #[arg(long)]
        no_stdin: bool,

        /// Do not inject `libdir` DirCap into the root environment.
        /// When set, [include libdir ...] fails with undefined variable.
        #[arg(long)]
        no_libdir: bool,

        /// Inject a named DirCap into the root environment (may be repeated).
        /// Format: NAME=PATH — binds $NAME to a DirCap for PATH.
        /// Example: --cap-fs data=/var/data injects $data as a DirCap for /var/data.
        #[arg(long, value_name = "NAME=PATH")]
        cap_fs: Vec<String>,

        /// Inject a named NetCap into the root environment (may be repeated).
        /// Format: NAME=ENTRY — binds $NAME to a NetCap.
        /// Multiple uses of the same NAME accumulate into one NetCap allowlist.
        /// Example: --cap-net api=api.internal --cap-net api=10.42.0.0/16
        #[arg(long, value_name = "NAME=ENTRY")]
        cap_net: Vec<String>,

        /// Input LLT files. Use `-` to read LLT source from stdin.
        /// Multiple files form a pipeline: each file's output becomes % for the next.
        files: Vec<String>,
    },
    /// Format LLT source code to canonical style.
    Fmt {
        /// Check formatting without writing changes (exit 1 if unformatted).
        #[arg(long)]
        check: bool,

        /// Write formatted output back to the file in place.
        #[arg(short, long)]
        in_place: bool,

        /// Input LLT file. Use `-` to read from stdin.
        file: String,
    },
    /// Compute and print the blake3 integrity hash of a file (for use with $include).
    Hash {
        /// File to hash.
        file: String,
    },
    /// Start an interactive REPL session.
    #[cfg(feature = "repl")]
    Repl,
    /// Start the LSP server (stdio transport).
    #[cfg(feature = "lsp")]
    Lsp,
    /// Show a detailed explanation for an error code (e.g. E001).
    Explain {
        /// Error code to explain (e.g. E001, E010, E070).
        code: String,
    },
    /// Extract and evaluate tinct code blocks embedded in a Markdown file.
    ///
    /// Treats each ```tinct or ```llt fenced code block as a pipeline stage.
    /// Blocks are connected with --- separators: % threads between them in order.
    Literate {
        /// Processing mode.
        mode: LiterateMode,
        /// Markdown file to process.
        file: String,
    },
}

/// Processing mode for `tinct literate`.
#[derive(Clone, ValueEnum)]
enum LiterateMode {
    /// Extract tinct code blocks and print as a ---‑separated pipeline source.
    Tangle,
    /// Extract blocks, evaluate the pipeline, and print the result as JSON.
    Eval,
    /// Evaluate blocks and output the Markdown with JSON results as comments after each block.
    Weave,
}

#[derive(Clone, ValueEnum)]
enum OutputFormat {
    /// JSON output (default).
    Json,
    /// LLT display format (Int(42), Dict({...}), etc.).
    Llt,
}

fn main() {
    let cli = Cli::parse();

    // Materialize is iterative (materialize_rc loop); no large worker stack needed.
    // The REPL spawns its own 128MB thread for eval when needed (src/repl.rs).
    let result = match cli.command {
        Commands::Eval {
            format,
            eval,
            no_fs,
            require_integrity,
            timeout,
            allow_path,
            no_landlock,
            max_memory,
            max_cpu,
            max_fds,
            no_env,
            allow_env,
            no_pwd,
            no_stdin,
            no_libdir,
            cap_fs,
            cap_net,
            files,
        } => run_eval(
            &files,
            &format,
            eval,
            no_fs,
            require_integrity,
            timeout.as_deref(),
            allow_path,
            no_landlock,
            max_memory,
            max_cpu,
            max_fds,
            no_env,
            allow_env,
            no_pwd,
            no_stdin,
            no_libdir,
            cap_fs,
            cap_net,
        ),
        Commands::Hash { file } => run_hash(&file),
        Commands::Fmt {
            check,
            in_place,
            file,
        } => run_fmt(&file, check, in_place),
        #[cfg(feature = "repl")]
        Commands::Repl => tinct::repl::run_repl(),
        #[cfg(feature = "lsp")]
        Commands::Lsp => tinct::lsp::run_lsp().map_err(|e| format!("{e}")),
        Commands::Explain { code } => run_explain(&code),
        Commands::Literate { mode, file } => run_literate(&file, &mode),
    };

    match result {
        Ok(()) => {}
        Err(e) => {
            eprintln!("{e}");
            process::exit(EXIT_ERROR);
        }
    }
}

/// Parse a duration string like "5s", "500ms", "2m" into seconds (u32).
/// Rounds up milliseconds to the nearest second (minimum 1).
fn parse_duration(s: &str) -> Result<u32, String> {
    let s = s.trim();

    // Try to parse with suffix
    if let Some(rest) = s.strip_suffix("ms") {
        let ms: u64 = rest
            .trim()
            .parse()
            .map_err(|_| format!("invalid duration: {s}"))?;
        // Round up to nearest second, minimum 1
        let secs = ms
            .checked_add(999)
            .ok_or_else(|| format!("duration out of range: {s}"))?
            / 1000;
        if secs == 0 || secs > u32::MAX as u64 {
            return Err(format!("duration out of range: {s}"));
        }
        return Ok(secs as u32);
    }

    if let Some(rest) = s.strip_suffix('s') {
        let secs: u32 = rest
            .trim()
            .parse()
            .map_err(|_| format!("invalid duration: {s}"))?;
        if secs == 0 {
            return Err("timeout must be at least 1 second".to_string());
        }
        return Ok(secs);
    }

    if let Some(rest) = s.strip_suffix('m') {
        let mins: u32 = rest
            .trim()
            .parse()
            .map_err(|_| format!("invalid duration: {s}"))?;
        if mins == 0 {
            return Err("timeout must be at least 1 second".to_string());
        }
        let secs = mins
            .checked_mul(60)
            .ok_or_else(|| format!("duration out of range: {s}"))?;
        return Ok(secs);
    }

    // No suffix — assume seconds
    let secs: u32 = s.parse().map_err(|_| format!("invalid duration: {s}"))?;
    if secs == 0 {
        return Err("timeout must be at least 1 second".to_string());
    }
    Ok(secs)
}

/// SIGALRM handler — exits with timeout code.
#[cfg(unix)]
extern "C" fn timeout_handler(_sig: i32) {
    unsafe { libc::_exit(EXIT_TIMEOUT) };
}

/// Install SIGALRM handler and start the alarm timer.
#[cfg(unix)]
fn install_timeout(duration_str: &str) -> Result<(), String> {
    let seconds = parse_duration(duration_str)?;

    unsafe {
        // Install signal handler using sigaction (more portable than signal())
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = timeout_handler as libc::sighandler_t;
        // SA_RESTART: restart syscalls interrupted by this signal (avoid EINTR)
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);

        if libc::sigaction(libc::SIGALRM, &sa, std::ptr::null_mut()) != 0 {
            return Err("failed to install SIGALRM handler".to_string());
        }

        // Start the alarm
        libc::alarm(seconds);
    }

    Ok(())
}

/// Apply rlimit resource caps (Unix only).
///
/// Sets RLIMIT_AS (virtual memory), RLIMIT_CPU (CPU time), and RLIMIT_NOFILE
/// (open file descriptors) via `libc::setrlimit`. These are process-wide hard
/// limits enforced by the kernel and cannot be raised by the process after being
/// set.
///
/// Default values are applied when the caller passes `None`:
/// - `max_memory`: 512 MB RLIMIT_AS limit (controls virtual address space; also
///   caps the maximum heap size the process can mmap).
/// - `max_cpu`: No limit by default (must be explicitly requested).
/// - `max_fds`: 64 RLIMIT_NOFILE (prevents FD exhaustion from crafted $include
///   chains; still leaves room for stdin/stdout/stderr + eval fds).
///
/// A value of `Some(0)` disables that particular limit.
#[cfg(unix)]
fn setup_rlimits(
    max_memory: Option<u64>,
    max_cpu: Option<u64>,
    max_fds: Option<u64>,
) -> Result<(), String> {
    // Helper: apply a single rlimit. Must be called within an unsafe block.
    // resource: the POSIX constant (RLIMIT_AS, RLIMIT_CPU, etc.)
    // limit_val: soft and hard limit value (same for both — we set a hard cap).
    // name: label for error messages.
    let apply_rlimit = |resource: libc::__rlimit_resource_t,
                        limit_val: libc::rlim_t,
                        name: &str|
     -> Result<(), String> {
        let rlim = libc::rlimit {
            rlim_cur: limit_val,
            rlim_max: limit_val,
        };
        let ret = unsafe { libc::setrlimit(resource as u32, &rlim) };
        if ret != 0 {
            return Err(format!(
                "failed to set {} limit to {}: {}",
                name,
                limit_val,
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    };

    // RLIMIT_AS: virtual address space limit.
    // Default: 512 MB. Prevents heap exhaustion from crafted inputs.
    // Value of 0 means: caller explicitly disabled this limit.
    let memory_limit = max_memory.unwrap_or(512 * 1024 * 1024) as libc::rlim_t;
    if memory_limit > 0 {
        apply_rlimit(libc::RLIMIT_AS, memory_limit, "RLIMIT_AS (max-memory)")?;
    }

    // RLIMIT_CPU: CPU time in seconds.
    // No default — only applied when explicitly requested.
    // This complements --timeout (wall-clock); RLIMIT_CPU limits compute time only.
    if let Some(cpu_secs) = max_cpu {
        if cpu_secs > 0 {
            apply_rlimit(
                libc::RLIMIT_CPU,
                cpu_secs as libc::rlim_t,
                "RLIMIT_CPU (max-cpu)",
            )?;
        }
    }

    // RLIMIT_NOFILE: open file descriptor count.
    // Default: 64 (leaves room for stdin/stdout/stderr + builtins + $include fds).
    // Value of 0 means: caller explicitly disabled this limit.
    let fd_limit = max_fds.unwrap_or(64) as libc::rlim_t;
    if fd_limit > 0 {
        apply_rlimit(libc::RLIMIT_NOFILE, fd_limit, "RLIMIT_NOFILE (max-fds)")?;
    }

    Ok(())
}

/// Apply seccomp-bpf network and process sandbox (Linux only).
///
/// Installs a BPF filter that blocks:
/// - Network syscalls: `socket`, `connect`, `bind`, `listen`, `accept`, `accept4`
/// - Process creation syscalls: `fork`, `vfork`, `execve`, `execveat`
///
/// The `clone` syscall is intentionally NOT blocked; the Rust runtime uses it
/// (with `CLONE_THREAD`) for thread creation. All other syscalls are allowed.
///
/// On architectures where a syscall does not exist (e.g. `fork`/`vfork` on
/// aarch64), the constant is simply absent from `libc`, so we gate each entry
/// behind a `#[cfg(target_arch)]` attribute.
///
/// Gracefully degrades: if seccompiler returns an error (e.g. the kernel is too
/// old or seccomp is disabled), the error is printed as a warning and evaluation
/// continues. This matches the Landlock degradation model used above.
#[cfg(target_os = "linux")]
fn setup_seccomp() -> Result<(), String> {
    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};
    use std::collections::BTreeMap;

    // Determine the target architecture for the BPF filter.
    // seccompiler requires this to match the running kernel's architecture.
    #[cfg(target_arch = "x86_64")]
    let arch = TargetArch::x86_64;
    #[cfg(target_arch = "aarch64")]
    let arch = TargetArch::aarch64;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        // seccompiler only supports x86_64 and aarch64. On other architectures
        // (e.g. arm, riscv64, s390x) we degrade gracefully without error.
        return Ok(());
    }

    // Build the syscall blocklist. Each entry maps a syscall number to an empty
    // rule vector, which means "match unconditionally" — the match_action
    // (EPERM) fires for every invocation of that syscall.
    let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();

    // Network syscalls — block all network socket operations.
    // LLT has no networking features; blocking these prevents any future
    // accidental or malicious use.
    for &nr in &[
        libc::SYS_socket,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
    ] {
        rules.insert(nr, vec![]);
    }

    // Process-creation syscalls — block spawning child processes.
    // execve/execveat prevent privilege-escalation via external binaries.
    // fork/vfork exist on x86_64; on aarch64 they are absent (clone covers both).
    for &nr in &[libc::SYS_execve, libc::SYS_execveat] {
        rules.insert(nr, vec![]);
    }

    // fork and vfork exist on x86_64 but not on aarch64.
    #[cfg(target_arch = "x86_64")]
    {
        rules.insert(libc::SYS_fork, vec![]);
        rules.insert(libc::SYS_vfork, vec![]);
    }

    // Build and compile the filter.
    // mismatch_action = Allow: all syscalls not in `rules` are permitted.
    // match_action = Errno(EPERM): blocked syscalls return EPERM to the caller.
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(|e| format!("seccomp: failed to build filter: {e}"))?;

    let bpf_prog: BpfProgram = filter
        .try_into()
        .map_err(|e| format!("seccomp: failed to compile filter: {e}"))?;

    // Apply the filter to all threads via TSYNC (ensures child threads also
    // inherit the filter, not just the calling thread).
    seccompiler::apply_filter_all_threads(&bpf_prog)
        .map_err(|e| format!("seccomp: failed to apply filter: {e}"))?;

    Ok(())
}

/// Apply Landlock filesystem ACL enforcement (Linux 5.13+ only, defense-in-depth).
///
/// When `--allow-path` entries are specified, the Landlock LSM is configured to
/// restrict the process to read-only access on the given paths. If the current
/// kernel does not support Landlock (older than 5.13, or the feature is disabled),
/// this function returns `Ok(())` without error — the application-level allowlist
/// (`EvalConfig.allowed_paths`) and cap-std remain the primary enforcement.
///
/// Landlock is applied as defense-in-depth: if a bug in the application-level check
/// allows an unauthorized path to reach `open()`, Landlock catches it at the kernel
/// level.
///
/// Note: Landlock does not eliminate TOCTOU races on its own (it checks paths at
/// `open()` time). The cap-std `RESOLVE_BENEATH` sandbox is the TOCTOU mitigation.
/// Landlock adds an independent kernel-level check.
///
/// Requires either `CAP_SYS_ADMIN` or `PR_SET_NO_NEW_PRIVS` (the latter is set
/// automatically by the landlock crate). Gracefully degrades on kernels < 5.13.
#[cfg(target_os = "linux")]
/// Set up Landlock filesystem sandbox.
///
/// `allowed_paths` — directories accessible for `$include` (and `--allow-path`).
/// `extra_readable` — additional directories that must be readable for the process to
///   function (e.g., the directories containing the main input files). These are NOT
///   added to the LLT-level allowlist; they only let the OS read the primary files.
fn setup_landlock(allowed_paths: &[PathBuf], extra_readable: &[PathBuf]) -> Result<(), String> {
    use landlock::{AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI};

    // V3 corresponds to Linux 5.19+. The crate gracefully degrades to a lower ABI
    // version if the running kernel doesn't support V3 (best-effort restriction).
    let abi = ABI::V3;

    // Build the initial ruleset with read-only filesystem access.
    let mut ruleset_created = Ruleset::default()
        .handle_access(AccessFs::from_read(abi))
        .map_err(|e| format!("landlock: failed to configure ruleset: {e}"))?
        .create()
        .map_err(|e| format!("landlock: failed to create ruleset: {e}"))?;

    // Add one PathBeneath rule for each allowed path (from --allow-path).
    // PathBeneath grants read access to the path and everything underneath it.
    for path in allowed_paths.iter().chain(extra_readable.iter()) {
        // Skip paths that don't exist (e.g., non-existent extra_readable dirs).
        if !path.exists() {
            continue;
        }
        let fd = PathFd::new(path).map_err(|e| {
            format!(
                "landlock: cannot open allowed path \"{}\": {e}",
                path.display()
            )
        })?;
        let rule = PathBeneath::new(fd, AccessFs::from_read(abi));
        ruleset_created = ruleset_created.add_rule(rule).map_err(|e| {
            format!(
                "landlock: failed to add rule for \"{}\": {e}",
                path.display()
            )
        })?;
    }

    // restrict_self() applies the ruleset to the current thread group.
    // On kernels < 5.13, this returns Ok(status) where status.ruleset is
    // NotEnforced — the call does not fail, it just has no effect.
    let _status = ruleset_created
        .restrict_self()
        .map_err(|e| format!("landlock: failed to restrict self: {e}"))?;

    Ok(())
}

/// Resolve the stdlib directory path from the binary location.
///
/// Tries two layouts in order:
/// 1. Development: `<exe_grandparent>/stdlib/`
/// 2. Installed: `<exe_parent>/../share/tinct/stdlib/`
///
/// Returns `None` if neither directory exists.
fn find_libdir_path() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.parent() // target/debug
                .and_then(|p| p.parent()) // target
                .and_then(|p| p.parent()) // project root
                .map(|root| root.join("stdlib"))
        })
        .filter(|p| p.is_dir())
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| {
                    exe.parent() // bin/
                        .and_then(|p| p.parent()) // <prefix>/
                        .map(|prefix| prefix.join("share").join("tinct").join("stdlib"))
                })
                .filter(|p| p.is_dir())
        })
}

/// Parse a CLI NetCap entry (from --cap-net NAME=ENTRY).
fn parse_cli_net_cap_entry(s: &str) -> Result<tinct::NetCapEntry, String> {
    use tinct::NetCapEntry;

    if let Some((host, port_str)) = s.split_once(':') {
        // host:port format
        let port: u16 = port_str
            .parse()
            .map_err(|_| format!("--cap-net: invalid port number '{}' in '{}'", port_str, s))?;
        Ok(NetCapEntry::HostPort(host.to_string(), port))
    } else if s.contains('*') {
        // Glob pattern (prefix wildcard only)
        if !s.starts_with("*.") {
            return Err(format!(
                "--cap-net: only prefix wildcards are supported (e.g., '*.internal'), got '{}'",
                s
            ));
        }
        Ok(NetCapEntry::HostnameGlob(s.to_string()))
    } else if s.contains('/') {
        // CIDR range — deferred to Phase 3
        Err(format!(
            "--cap-net: CIDR ranges are not yet implemented (got '{}')",
            s
        ))
    } else {
        // Plain hostname
        Ok(NetCapEntry::Hostname(s.to_string()))
    }
}

fn run_eval(
    file_paths: &[String],
    format: &OutputFormat,
    force_eval: bool,
    no_fs: bool,
    require_integrity: bool,
    timeout: Option<&str>,
    allow_path: Vec<PathBuf>,
    no_landlock: bool,
    max_memory: Option<u64>,
    max_cpu: Option<u64>,
    max_fds: Option<u64>,
    no_env: bool,
    allow_env: Vec<String>,
    no_pwd: bool,
    no_stdin: bool,
    no_libdir: bool,
    cap_fs: Vec<String>,
    cap_net: Vec<String>,
) -> Result<(), String> {
    if file_paths.is_empty() {
        return Err("no input files specified".to_string());
    }
    // Install timeout handler if requested (must happen before evaluation)
    if let Some(duration) = timeout {
        #[cfg(unix)]
        {
            install_timeout(duration)?;
        }
        #[cfg(not(unix))]
        {
            eprintln!("error: --timeout is only supported on Unix platforms");
            process::exit(EXIT_ERROR);
        }
    }

    // Apply rlimit resource caps (Unix only). Must happen early, before any
    // significant allocation, so that any heap limit is immediately enforced.
    #[cfg(unix)]
    setup_rlimits(max_memory, max_cpu, max_fds)?;
    // On non-Unix platforms, rlimit flags are accepted for CLI compatibility
    // but have no effect (POSIX rlimits are not available).
    #[cfg(not(unix))]
    {
        let _ = max_memory;
        let _ = max_cpu;
        let _ = max_fds;
    }

    // Check for piped stdin JSON (only when first file is not stdin itself).
    // Returns raw serde_json::Value; conversion to LLT happens after the first
    // EvalContext is created so ThunkIds are allocated in the shared arena.
    let stdin_json = if file_paths[0] != "-" {
        read_stdin_json()?
    } else {
        None
    };

    // Create stdlib environment
    let env = create_stdlib_env().map_err(|e| format!("{e}"))?;

    // Canonicalize --allow-path entries at startup so comparisons are stable.
    // Non-existent paths are rejected immediately with a clear error message.
    let canonical_allowed_paths: Vec<PathBuf> = allow_path
        .iter()
        .map(|p| {
            p.canonicalize().map_err(|e| {
                format!(
                    "--allow-path: cannot canonicalize \"{}\": {}",
                    p.display(),
                    e
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Apply Landlock filesystem ACL enforcement (Linux only, defense-in-depth).
    // Must be called after path canonicalization so the allowed paths are stable.
    // Only activated when --allow-path is given and --no-landlock is not set.
    //
    // Also grant read access to the directories containing the main input files so
    // they can be read before evaluation starts. These extra-readable dirs are NOT
    // added to the LLT-level allowlist (canonical_allowed_paths) — the allowlist only
    // restricts $include resolution, not the primary files.
    #[cfg(target_os = "linux")]
    if !no_landlock && !canonical_allowed_paths.is_empty() {
        // Collect the canonical parent directories of each input file.
        let extra_readable: Vec<PathBuf> = file_paths
            .iter()
            .filter(|p| p.as_str() != "-")
            .filter_map(|p| {
                let path = std::path::Path::new(p.as_str());
                let dir = match path.parent().filter(|d| !d.as_os_str().is_empty()) {
                    Some(d) => d.to_path_buf(),
                    None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                };
                dir.canonicalize().ok()
            })
            .collect();
        setup_landlock(&canonical_allowed_paths, &extra_readable)?;
    }
    // On non-Linux platforms, --no-landlock is accepted for CLI compatibility
    // but has no effect (Landlock is a Linux-only API).
    #[cfg(not(target_os = "linux"))]
    let _ = no_landlock;

    // Install seccomp-bpf network and process sandbox (Linux only).
    // Applied after Landlock so that both kernel-level defenses are active before
    // eval. Gracefully degrades on unsupported kernels (prints warning, continues).
    #[cfg(target_os = "linux")]
    if let Err(e) = setup_seccomp() {
        eprintln!("warning: seccomp sandbox not active: {e}");
    }

    // Inject `pwd` DirCap into the root environment (unless --no-pwd is set).
    // --no-pwd enforcement: when the flag is set, `pwd` is NOT injected, so
    // any reference to `pwd` in the program will fail with "undefined variable".
    // This is the correct enforcement mechanism — no special runtime checks needed.
    if !no_pwd {
        use tinct::Value;
        let pwd_path = std::env::current_dir()
            .map_err(|e| format!("cannot determine working directory for pwd: {e}"))?;
        let pwd_dir = cap_std::fs::Dir::open_ambient_dir(&pwd_path, cap_std::ambient_authority())
            .map_err(|e| format!("cannot open pwd directory: {e}"))?;
        let pwd_value = Value::DirCap(Rc::new(pwd_dir));
        let pwd_thunk = tinct::Thunk::new_materialized(pwd_value, tinct::Span::origin());
        env.borrow_mut()
            .insert("pwd".to_string(), Rc::new(pwd_thunk));
    }

    // Inject `stdin` Handle for fd 0 into the root environment (unless --no-stdin is set).
    // --no-stdin enforcement: when the flag is set, `stdin` is NOT injected, so
    // any reference to `stdin` in the program will fail with "undefined variable".
    if !no_stdin {
        use std::cell::RefCell;
        use std::io::BufReader;
        use tinct::Value;
        let stdin_handle = Value::Handle(Rc::new(RefCell::new(Box::new(BufReader::new(
            std::io::stdin(),
        ))
            as Box<dyn std::io::BufRead>)));
        let stdin_thunk = tinct::Thunk::new_materialized(stdin_handle, tinct::Span::origin());
        env.borrow_mut()
            .insert("stdin".to_string(), Rc::new(stdin_thunk));
    }

    // Inject `libdir` DirCap for the stdlib directory (unless --no-libdir is set).
    // --no-libdir enforcement: when the flag is set, `libdir` is NOT injected, so
    // any reference to `libdir` in the program will fail with "undefined variable".
    // Phase 1: resolve libdir from the binary's location or a well-known relative path.
    // If resolution fails, libdir is not injected (stdlib is embedded at compile time anyway).
    // The resolved path is also saved for the JSON output path (format_with_json_llt).
    let resolved_libdir_path: Option<std::path::PathBuf> =
        if !no_libdir { find_libdir_path() } else { None };
    if !no_libdir {
        use tinct::Value;
        if let Some(ref path) = resolved_libdir_path {
            if let Ok(libdir_std) =
                cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())
            {
                let libdir_value = Value::DirCap(Rc::new(libdir_std));
                let libdir_thunk =
                    tinct::Thunk::new_materialized(libdir_value, tinct::Span::origin());
                env.borrow_mut()
                    .insert("libdir".to_string(), Rc::new(libdir_thunk));
            }
            // If the dir can't be opened, silently skip — stdlib is embedded anyway.
        }
        // TODO(io-phase2): --libdir-path PATH override for custom installations
    }

    // Inject --cap-fs NAME=PATH entries into the root environment
    {
        use tinct::Value;
        for cap_fs_entry in &cap_fs {
            let (name, path_str) = cap_fs_entry.split_once('=').ok_or_else(|| {
                format!(
                    "--cap-fs: expected NAME=PATH format, got {:?}",
                    cap_fs_entry
                )
            })?;
            let name = name.trim();
            if name.is_empty() {
                return Err(format!(
                    "--cap-fs: NAME must not be empty in {:?}",
                    cap_fs_entry
                ));
            }
            let cap_path = std::path::Path::new(path_str.trim());
            let cap_dir =
                cap_std::fs::Dir::open_ambient_dir(cap_path, cap_std::ambient_authority())
                    .map_err(|e| {
                        format!(
                            "--cap-fs: cannot open directory {:?}: {e}",
                            cap_path.display()
                        )
                    })?;
            let cap_value = Value::DirCap(Rc::new(cap_dir));
            let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
            env.borrow_mut()
                .insert(name.to_string(), Rc::new(cap_thunk));
        }
    }

    // Inject --cap-net NAME=ENTRY entries into the root environment
    // Multiple uses of the same NAME accumulate into one NetCap allowlist
    {
        use std::collections::HashMap;
        use tinct::NetCapEntry;
        use tinct::Value;

        let mut net_caps: HashMap<String, Vec<NetCapEntry>> = HashMap::new();

        for cap_net_entry in &cap_net {
            let (name, entry_str) = cap_net_entry.split_once('=').ok_or_else(|| {
                format!(
                    "--cap-net: expected NAME=ENTRY format, got {:?}",
                    cap_net_entry
                )
            })?;
            let name = name.trim();
            if name.is_empty() {
                return Err(format!(
                    "--cap-net: NAME must not be empty in {:?}",
                    cap_net_entry
                ));
            }
            let entry_str = entry_str.trim();

            // Parse the entry using the same logic as builtin_net_cap
            let entry = parse_cli_net_cap_entry(entry_str)?;
            net_caps.entry(name.to_string()).or_default().push(entry);
        }

        // Create NetCap values and inject them
        for (name, entries) in net_caps {
            let cap_value = Value::NetCap(Rc::new(entries));
            let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
            env.borrow_mut().insert(name, Rc::new(cap_thunk));
        }
    }

    // Determine env_allowed based on CLI flags.
    // --no-env and --allow-env enforcement: the `env` builtin checks this field
    // at runtime (see builtin_env in builtins.rs). Returns Null for disallowed vars.
    // None = unrestricted, Some(empty) = all denied (--no-env), Some(set) = only those allowed
    let env_allowed = if no_env {
        Some(std::collections::HashSet::new()) // empty set = all denied
    } else if !allow_env.is_empty() {
        Some(allow_env.into_iter().collect()) // specific allowlist
    } else {
        None // unrestricted
    };

    // Multi-file pipeline: process each file in sequence, passing output as input to the next.
    //
    // ARENA SHARING INVARIANT: All files in the pipeline must share the same ThunkArena so
    // that ThunkIds allocated by earlier files remain valid when later files reference them
    // via the `%` pipeline variable. We establish one base EvalContext for the first file,
    // then use `with_base_dir_and_path` for subsequent files — this creates a new config
    // (different base_dir) while sharing the same arena, state, and stdlib_env.
    let mut pipeline_input: Option<Rc<Thunk>> = None;

    let mut thunk = None;
    let mut last_source = String::new();
    let mut last_eval_ctx: Option<Rc<EvalContext>> = None;
    let mut base_eval_ctx: Option<Rc<EvalContext>> = None;

    for file_path in file_paths {
        // Read the LLT source
        let source = read_source(file_path)?;

        // Parse
        let mut ast = parse(&source).map_err(|e| format!("{e}"))?;

        // PIPELINE INVARIANT: Desugar must run after parse and before typecheck.
        // See also: src/lib.rs:87-91 (eval_source_with_config pipeline)
        // Desugar $_ implicit lambdas (mandatory pre-typecheck AST transformation).
        tinct::desugar::desugar_file(&mut ast.node);

        // Variable resolution pass (Phase 1 of arena allocation strategy).
        tinct::resolve::resolve_file(&ast.node);

        // Type errors are advisory; evaluation proceeds regardless.
        let _ = tinct::typecheck::typecheck_file(&ast.node);

        // Determine base directory for $include resolution
        let file_base_dir_path = if file_path == "-" {
            std::env::current_dir()
                .map_err(|e| format!("cannot determine working directory: {e}"))?
        } else {
            let p = std::path::Path::new(file_path);
            // Use the file's parent directory; fall back to cwd if the path has no parent
            // (e.g., a bare filename like "test.llt").
            match p.parent().filter(|d| !d.as_os_str().is_empty()) {
                Some(dir) => dir
                    .canonicalize()
                    .map_err(|e| format!("cannot resolve directory for \"{file_path}\": {e}"))?,
                None => std::env::current_dir()
                    .map_err(|e| format!("cannot determine working directory: {e}"))?,
            }
        };

        // Open base_dir as a cap-std Dir
        let base_dir =
            cap_std::fs::Dir::open_ambient_dir(&file_base_dir_path, cap_std::ambient_authority())
                .map_err(|e| format!("cannot open base directory: {e}"))?;

        // Create or derive the evaluation context.
        // First file: create the base context (owns the ThunkArena).
        // Subsequent files: derive from the base context via with_base_dir_and_path so all
        // files share the same arena — ThunkIds from earlier files remain valid in later ones.
        let eval_ctx = if let Some(ref base) = base_eval_ctx {
            base.with_base_dir_and_path(base_dir, Some(file_base_dir_path.clone()))
        } else {
            let ctx = EvalContext::new_with_full_options(
                base_dir,
                Some(file_base_dir_path.clone()),
                Rc::clone(&env),
                no_fs,
                require_integrity,
                canonical_allowed_paths.clone(),
                env_allowed.clone(),
            );
            // Convert stdin JSON using this context so ThunkIds go into the shared arena.
            if let Some(ref json) = stdin_json {
                let thunk_val =
                    json_to_value(json, 0, Span::origin(), &ctx).map_err(|e| format!("{e}"))?;
                pipeline_input = Some(thunk_val);
            }
            base_eval_ctx = Some(Rc::clone(&ctx));
            ctx
        };

        // Evaluate file with pipeline input
        let file_result =
            eval_file_with_input(&ast.node, Rc::clone(&env), &eval_ctx, pipeline_input, 0)
                .map_err(|e| {
                    let mut error_str = format!("{e}");
                    if let Some(snippet) = tinct::render_span_snippet(&source, e.definition_span) {
                        error_str.push('\n');
                        error_str.push_str(&snippet);
                    }
                    error_str
                })?;

        // Pass the result as lazy thunk to next file (matching --- boundary semantics).
        // Because all files share the same ThunkArena, the ThunkIds in file_result are
        // valid in the next file's eval context.
        pipeline_input = Some(file_result.clone());

        // Keep track of the last file's result, source, and context for final output.
        // IMPORTANT: The ThunkIds in the result's Value::Dict map are indices into the
        // shared ThunkArena. We MUST use an eval_ctx backed by the same arena for
        // value_to_json; since all file contexts share the arena, any of them works.
        thunk = Some(file_result);
        last_source = source;
        last_eval_ctx = Some(eval_ctx);
    }

    let thunk = thunk.ok_or_else(|| "internal error: no files processed".to_string())?;
    let eval_ctx = last_eval_ctx.ok_or_else(|| "internal error: no eval context".to_string())?;

    // Materialize the final result
    let val = materialize(&thunk, None, &eval_ctx, 0).map_err(|e| {
        let mut error_str = format!("{e}");
        if let Some(snippet) = tinct::render_span_snippet(&last_source, e.definition_span) {
            error_str.push('\n');
            error_str.push_str(&snippet);
        }
        error_str
    })?;

    // Optionally deep-force all thunks
    let val = if force_eval {
        deep_materialize(&val, &eval_ctx, 0, None).map_err(|e| {
            let mut error_str = format!("{e}");
            if let Some(snippet) = tinct::render_span_snippet(&last_source, e.definition_span) {
                error_str.push('\n');
                error_str.push_str(&snippet);
            }
            error_str
        })?
    } else {
        val
    };

    // Handle top-level Seq value.
    //
    // A Seq at the top level has two valid interpretations depending on whether
    // emit was called:
    //
    // 1. emitted=true (generator + emit pattern): drain the Seq by forcing each
    //    element's tail to completion. This drives any emit side-effects inside
    //    the generator elements. Element values themselves are discarded.
    //
    // 2. emitted=false (bare Seq with no text output): this is almost certainly
    //    a mistake — the user forgot to collect or emit. Return a clear error
    //    rather than silently failing to produce output.
    if matches!(val, tinct::Value::Seq { .. }) {
        if eval_ctx.emitted.get() {
            // Drive the Seq to completion so all emit calls inside generator elements fire.
            // We force each head (which triggers emit side-effects) then advance to the tail.
            // This mirrors builtin_collect's spine traversal but discards the collected values.
            let mut current = val;
            let mut drain_count: usize = 0;
            loop {
                match current {
                    tinct::Value::Seq { head, tail } => {
                        // Enforce element limit to prevent unbounded CPU/memory consumption
                        // from infinite sequences (mirrors MAX_COLLECT_SIZE in builtins_seq_prim.rs).
                        if drain_count >= MAX_COLLECT_SIZE {
                            return Err(format!(
                                "top-level Seq drain exceeded maximum collection size ({}). Use $take to limit infinite sequences.",
                                MAX_COLLECT_SIZE
                            ));
                        }
                        drain_count += 1;
                        // Force the head to trigger any emit calls inside it.
                        let head_thunk = eval_ctx.get_thunk(head);
                        materialize(&head_thunk, None, &eval_ctx, 0).map_err(|e| {
                            let mut error_str = format!("{e}");
                            if let Some(snippet) =
                                tinct::render_span_snippet(&last_source, e.definition_span)
                            {
                                error_str.push('\n');
                                error_str.push_str(&snippet);
                            }
                            error_str
                        })?;
                        // Advance to the tail.
                        let tail_thunk = eval_ctx.get_thunk(tail);
                        current = materialize(&tail_thunk, None, &eval_ctx, 0).map_err(|e| {
                            let mut error_str = format!("{e}");
                            if let Some(snippet) =
                                tinct::render_span_snippet(&last_source, e.definition_span)
                            {
                                error_str.push('\n');
                                error_str.push_str(&snippet);
                            }
                            error_str
                        })?;
                    }
                    tinct::Value::Dict(ref d) if d.is_empty() => break,
                    other => {
                        return Err(format!(
                            "top-level Seq has malformed tail: expected Seq or [] but got {}",
                            other.type_name()
                        ));
                    }
                }
            }
        } else {
            return Err(
                "top-level Seq — use '| collect' for JSON array output or 'emit' for text output"
                    .to_string(),
            );
        }
        // Cancel any pending alarm before returning
        #[cfg(unix)]
        if timeout.is_some() {
            unsafe {
                libc::alarm(0);
            }
        }
        return Ok(());
    }

    // Serialize and output (skip if emit was called)
    if !eval_ctx.emitted.get() {
        match format {
            OutputFormat::Json => {
                // Try stdlib/fmt/json.llt for pure-tinct JSON serialization.
                // Falls back to value_to_json() when json.llt is not found (e.g., no libdir).
                let json_llt_path = resolved_libdir_path
                    .as_deref()
                    .map(|p| p.join("fmt").join("json.llt"));

                let output = if let Some(ref json_llt_path) = json_llt_path {
                    match format_with_json_llt(
                        Rc::clone(&thunk),
                        &eval_ctx,
                        Rc::clone(&env),
                        json_llt_path,
                    ) {
                        Ok(Some(compact_json)) => {
                            // json.llt produces compact JSON. Parse and re-serialize as
                            // pretty-printed to preserve the expected output format.
                            let parsed: serde_json::Value = serde_json::from_str(&compact_json)
                                .map_err(|e| {
                                    format!("json.llt produced invalid JSON: {e}\noutput: {compact_json}")
                                })?;
                            serde_json::to_string_pretty(&parsed)
                                .map_err(|e| format!("JSON pretty-print error: {e}"))?
                        }
                        Ok(None) => {
                            // json.llt not found — fall back to Rust serializer.
                            let json = value_to_json(&val, &eval_ctx, 0).map_err(|e| {
                                let mut error_str = format!("{e}");
                                if let Some(snippet) =
                                    tinct::render_span_snippet(&last_source, e.definition_span)
                                {
                                    error_str.push('\n');
                                    error_str.push_str(&snippet);
                                }
                                error_str
                            })?;
                            serde_json::to_string_pretty(&json)
                                .map_err(|e| format!("JSON serialization error: {e}"))?
                        }
                        Err(e) => return Err(e),
                    }
                } else {
                    // No libdir resolved — fall back to Rust serializer.
                    let json = value_to_json(&val, &eval_ctx, 0).map_err(|e| {
                        let mut error_str = format!("{e}");
                        if let Some(snippet) =
                            tinct::render_span_snippet(&last_source, e.definition_span)
                        {
                            error_str.push('\n');
                            error_str.push_str(&snippet);
                        }
                        error_str
                    })?;
                    serde_json::to_string_pretty(&json)
                        .map_err(|e| format!("JSON serialization error: {e}"))?
                };
                println!("{output}");
            }
            OutputFormat::Llt => {
                // Deep-materialize for display (value_to_display_string needs it).
                // Skip if --eval already deep-materialized above.
                let display_val = if force_eval {
                    &val
                } else {
                    &deep_materialize(&val, &eval_ctx, 0, None).map_err(|e| {
                        let mut error_str = format!("{e}");
                        if let Some(snippet) =
                            tinct::render_span_snippet(&last_source, e.definition_span)
                        {
                            error_str.push('\n');
                            error_str.push_str(&snippet);
                        }
                        error_str
                    })?
                };
                let output = value_to_display_string(display_val, &eval_ctx, 0).map_err(|e| {
                    let mut error_str = format!("{e}");
                    if let Some(snippet) =
                        tinct::render_span_snippet(&last_source, e.definition_span)
                    {
                        error_str.push('\n');
                        error_str.push_str(&snippet);
                    }
                    error_str
                })?;
                println!("{output}");
            }
        }
    }

    // Cancel any pending alarm before returning success
    #[cfg(unix)]
    if timeout.is_some() {
        unsafe {
            libc::alarm(0);
        }
    }

    Ok(())
}

fn run_fmt(file_path: &str, check: bool, in_place: bool) -> Result<(), String> {
    let source = read_source(file_path)?;
    let formatted = format_source(&source).map_err(|e| format!("{e}"))?;

    if check {
        if source != formatted {
            if file_path == "-" {
                return Err("stdin: not formatted".to_string());
            }
            return Err(format!("{file_path}: not formatted"));
        }
        return Ok(());
    }

    if in_place {
        if file_path == "-" {
            return Err("--in-place cannot be used with stdin".to_string());
        }
        std::fs::write(file_path, &formatted)
            .map_err(|e| format!("error writing {file_path}: {e}"))?;
        return Ok(());
    }

    print!("{formatted}");
    Ok(())
}

/// Compute the blake3 hash of a file and print `blake3:<hexdigest>`.
/// Used to generate integrity hashes for `$include` second arguments.
fn run_hash(file_path: &str) -> Result<(), String> {
    let file = std::fs::File::open(file_path).map_err(|e| format!("error reading file: {e}"))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("error reading file: {e}"))?;
    if metadata.len() > MAX_FILE_SIZE {
        return Err(format!(
            "file is {} bytes, exceeds the 10 MB limit",
            metadata.len()
        ));
    }
    let mut buf = Vec::new();
    file.take(MAX_FILE_SIZE + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("error reading file: {e}"))?;
    let hash = blake3::hash(&buf);
    println!("blake3:{}", hash.to_hex());
    Ok(())
}

/// Read LLT source from a file path or stdin (when path is `-`).
fn read_source(file_path: &str) -> Result<String, String> {
    if file_path == "-" {
        let mut buf = String::new();
        io::stdin()
            .take(MAX_FILE_SIZE + 1)
            .read_to_string(&mut buf)
            .map_err(|e| format!("error reading stdin: {e}"))?;
        if buf.len() as u64 > MAX_FILE_SIZE {
            return Err(format!(
                "stdin input exceeds the 10 MB limit ({} bytes)",
                MAX_FILE_SIZE
            ));
        }
        Ok(buf)
    } else {
        // Open the file first to get a stable fd, avoiding TOCTOU race.
        let file =
            std::fs::File::open(file_path).map_err(|e| format!("error reading file: {e}"))?;
        let metadata = file
            .metadata()
            .map_err(|e| format!("error reading file: {e}"))?;
        if metadata.len() > MAX_FILE_SIZE {
            return Err(format!(
                "input file is {} bytes, which exceeds the 10 MB limit ({} bytes)",
                metadata.len(),
                MAX_FILE_SIZE
            ));
        }
        // Read from the open fd using take() to limit reads at the OS level.
        let mut buf = String::new();
        file.take(MAX_FILE_SIZE + 1)
            .read_to_string(&mut buf)
            .map_err(|e| format!("error reading file: {e}"))?;
        if buf.len() as u64 > MAX_FILE_SIZE {
            return Err(format!(
                "file grew beyond 10 MB limit during read ({} bytes)",
                buf.len()
            ));
        }
        Ok(buf)
    }
}

/// If stdin is not a terminal (i.e., data is piped), read it as JSON and convert
/// to an LLT Value for injection as `%` in the first document.
/// Read and parse stdin JSON. Returns the raw JSON value (not yet converted to LLT).
/// The caller must convert it using `json_to_value` with the evaluation context so
/// ThunkIds are allocated in the correct arena.
fn read_stdin_json() -> Result<Option<serde_json::Value>, String> {
    if io::stdin().is_terminal() {
        return Ok(None);
    }

    let mut buf = String::new();
    io::stdin()
        .take(MAX_FILE_SIZE + 1)
        .read_to_string(&mut buf)
        .map_err(|e| format!("error reading stdin: {e}"))?;

    if buf.len() as u64 > MAX_FILE_SIZE {
        return Err(format!(
            "stdin JSON input exceeds the 10 MB limit ({} bytes)",
            MAX_FILE_SIZE
        ));
    }

    if buf.trim().is_empty() {
        return Ok(None);
    }

    let json: serde_json::Value =
        serde_json::from_str(&buf).map_err(|e| format!("error parsing stdin JSON: {e}"))?;

    Ok(Some(json))
}

/// Process a Markdown file in literate mode.
///
/// Extracts ```` ```tinct ```` and ```` ```llt ```` fenced code blocks and
/// handles them according to `mode`:
///
/// - **`tangle`** — print the extracted blocks joined with `\n---\n`.
/// - **`eval`** — join the blocks, evaluate the resulting pipeline, print JSON.
/// - **`weave`** — evaluate each block in pipeline order; output the original
///   Markdown with the JSON result appended as a comment after each tinct block.
fn run_literate(file_path: &str, mode: &LiterateMode) -> Result<(), String> {
    let markdown = read_source(file_path)?;
    let blocks = literate::extract_code_blocks(&markdown);

    if blocks.is_empty() {
        match mode {
            LiterateMode::Tangle => {
                // Nothing to print — output empty string (no trailing newline).
                return Ok(());
            }
            LiterateMode::Eval => {
                return Err("no tinct code blocks found in the Markdown file".to_string());
            }
            LiterateMode::Weave => {
                // Nothing to annotate — print the Markdown unchanged.
                print!("{markdown}");
                return Ok(());
            }
        }
    }

    match mode {
        LiterateMode::Tangle => {
            let tangled = literate::tangle(blocks);
            println!("{tangled}");
            Ok(())
        }

        LiterateMode::Eval => {
            let tangled = literate::tangle(blocks);
            run_literate_eval(&tangled, file_path)
        }

        LiterateMode::Weave => run_literate_weave(&markdown, &blocks, file_path),
    }
}

/// Evaluate a tangled tinct source string and print the result as JSON.
///
/// Reuses the same pipeline as `run_eval` (parse → desugar → resolve →
/// typecheck → eval → materialize → JSON), with no sandbox flags applied.
/// The base directory is derived from the Markdown file's parent directory.
fn run_literate_eval(tangled: &str, markdown_path: &str) -> Result<(), String> {
    // Parse the tangled source.
    let mut ast =
        parse(tangled).map_err(|e| format!("parse error in tangled tinct source: {e}"))?;

    tinct::desugar::desugar_file(&mut ast.node);
    tinct::resolve::resolve_file(&ast.node);
    let _ = tinct::typecheck::typecheck_file(&ast.node);

    // Determine base directory from the Markdown file's location.
    let base_dir_path = if markdown_path == "-" {
        std::env::current_dir().map_err(|e| format!("cannot determine working directory: {e}"))?
    } else {
        let p = std::path::Path::new(markdown_path);
        match p.parent().filter(|d| !d.as_os_str().is_empty()) {
            Some(dir) => dir
                .canonicalize()
                .map_err(|e| format!("cannot resolve directory for \"{markdown_path}\": {e}"))?,
            None => std::env::current_dir()
                .map_err(|e| format!("cannot determine working directory: {e}"))?,
        }
    };

    let base_dir = cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
        .map_err(|e| format!("cannot open base directory: {e}"))?;

    let env = create_stdlib_env().map_err(|e| format!("{e}"))?;

    let eval_ctx = EvalContext::new(base_dir, Rc::clone(&env), false);

    let thunk =
        eval_file_with_input(&ast.node, Rc::clone(&env), &eval_ctx, None, 0).map_err(|e| {
            let mut msg = format!("{e}");
            if let Some(snippet) = tinct::render_span_snippet(tangled, e.definition_span) {
                msg.push('\n');
                msg.push_str(&snippet);
            }
            msg
        })?;

    let val = materialize(&thunk, None, &eval_ctx, 0).map_err(|e| {
        let mut msg = format!("{e}");
        if let Some(snippet) = tinct::render_span_snippet(tangled, e.definition_span) {
            msg.push('\n');
            msg.push_str(&snippet);
        }
        msg
    })?;

    // Respect emit: if any emit call fired during eval, suppress JSON output.
    if !eval_ctx.emitted.get() {
        // Use the same format_with_json_llt → fallback pattern as run_eval for consistent
        // null semantics ([] → JSON null, not {}).
        let json_llt_path = find_libdir_path().map(|p| p.join("fmt").join("json.llt"));

        let output = if let Some(ref json_llt_path) = json_llt_path {
            match format_with_json_llt(
                Rc::clone(&thunk),
                &eval_ctx,
                Rc::clone(&env),
                json_llt_path,
            ) {
                Ok(Some(compact_json)) => {
                    let parsed: serde_json::Value =
                        serde_json::from_str(&compact_json).map_err(|e| {
                            format!(
                                "json.llt produced invalid JSON: {e}\noutput: {compact_json}"
                            )
                        })?;
                    serde_json::to_string_pretty(&parsed)
                        .map_err(|e| format!("JSON pretty-print error: {e}"))?
                }
                Ok(None) => {
                    let json = value_to_json(&val, &eval_ctx, 0).map_err(|e| format!("{e}"))?;
                    serde_json::to_string_pretty(&json)
                        .map_err(|e| format!("JSON serialization error: {e}"))?
                }
                Err(e) => return Err(e),
            }
        } else {
            let json = value_to_json(&val, &eval_ctx, 0).map_err(|e| format!("{e}"))?;
            serde_json::to_string_pretty(&json)
                .map_err(|e| format!("JSON serialization error: {e}"))?
        };

        println!("{output}");
    }

    Ok(())
}

/// Weave mode: output the Markdown with JSON results appended after each tinct block.
///
/// For Phase 4, weave is a simplified implementation: it evaluates the tinct pipeline
/// formed by all blocks in order and appends the final JSON result as a comment
/// (`` <!-- tinct-result: ... --> ``) immediately after each closing `` ``` `` fence.
/// Each block is evaluated in pipeline order — `%` from block N becomes the input
/// to block N+1 — matching the tangle/eval semantics exactly.
///
/// Full result substitution (replacing inline markers like `<!-- tinct: expr -->`)
/// is a future refinement.
fn run_literate_weave(
    markdown: &str,
    blocks: &[String],
    markdown_path: &str,
) -> Result<(), String> {
    // Evaluate the pipeline incrementally: process one block at a time, threading
    // % between them. This lets us annotate each block with the result at that point.
    let base_dir_path = if markdown_path == "-" {
        std::env::current_dir().map_err(|e| format!("cannot determine working directory: {e}"))?
    } else {
        let p = std::path::Path::new(markdown_path);
        match p.parent().filter(|d| !d.as_os_str().is_empty()) {
            Some(dir) => dir
                .canonicalize()
                .map_err(|e| format!("cannot resolve directory for \"{markdown_path}\": {e}"))?,
            None => std::env::current_dir()
                .map_err(|e| format!("cannot determine working directory: {e}"))?,
        }
    };

    let env = create_stdlib_env().map_err(|e| format!("{e}"))?;

    // Create one base EvalContext that owns the shared ThunkArena.
    // All blocks derive from this context via with_base_dir_and_path so that
    // ThunkIds allocated by block N remain valid when block N+1 references them
    // via the % pipeline variable. This matches the arena-sharing pattern used by
    // the multi-file pipeline in run_eval.
    let base_dir_initial =
        cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
            .map_err(|e| format!("cannot open base directory: {e}"))?;
    let base_eval_ctx = EvalContext::new(base_dir_initial, Rc::clone(&env), false);

    // Evaluate each block in turn, passing the previous result as pipeline input.
    // Collect (block_index -> JSON result) for annotation.
    let mut pipeline_input: Option<Rc<Thunk>> = None;
    let mut block_results: Vec<String> = Vec::with_capacity(blocks.len());

    for (i, block) in blocks.iter().enumerate() {
        let mut ast =
            parse(block).map_err(|e| format!("parse error in code block {}: {e}", i + 1))?;
        tinct::desugar::desugar_file(&mut ast.node);
        tinct::resolve::resolve_file(&ast.node);
        let _ = tinct::typecheck::typecheck_file(&ast.node);

        // Derive per-block context from the base context (shares the ThunkArena).
        let base_dir =
            cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
                .map_err(|e| format!("cannot open base directory: {e}"))?;

        let eval_ctx = base_eval_ctx.with_base_dir_and_path(base_dir, Some(base_dir_path.clone()));

        let thunk = eval_file_with_input(
            &ast.node,
            Rc::clone(&env),
            &eval_ctx,
            pipeline_input.clone(),
            0,
        )
        .map_err(|e| format!("error in code block {}: {e}", i + 1))?;

        let val = materialize(&thunk, None, &eval_ctx, 0)
            .map_err(|e| format!("error materializing code block {}: {e}", i + 1))?;

        let json_str = if eval_ctx.emitted.get() {
            // Block called emit — note this in the annotation.
            "(emit)".to_string()
        } else {
            let json = value_to_json(&val, &eval_ctx, 0)
                .map_err(|e| format!("error serializing code block {} result: {e}", i + 1))?;
            serde_json::to_string(&json)
                .map_err(|e| format!("JSON serialization error in block {}: {e}", i + 1))?
        };

        block_results.push(json_str);
        // Thread the result as pipeline input to the next block.
        pipeline_input = Some(Rc::clone(&thunk));
    }

    // Now walk the Markdown and insert result comments after each tinct/llt block.
    let mut block_idx = 0;
    let mut in_tinct_block = false;
    let mut output = String::with_capacity(markdown.len() + block_results.len() * 80);

    for line in markdown.lines() {
        let trimmed = line.trim();
        output.push_str(line);
        output.push('\n');

        if !in_tinct_block {
            if trimmed == "```tinct" || trimmed == "```llt" {
                in_tinct_block = true;
            }
        } else if trimmed == "```" {
            // Closing fence: append the result comment for this block.
            in_tinct_block = false;
            if block_idx < block_results.len() {
                output.push_str(&format!(
                    "<!-- tinct-result: {} -->\n",
                    block_results[block_idx]
                ));
                block_idx += 1;
            }
        }
    }

    print!("{output}");
    Ok(())
}

/// Print a detailed explanation for the given error code string (e.g. "E001").
/// The code is matched case-insensitively after stripping leading/trailing whitespace.
fn run_explain(code: &str) -> Result<(), String> {
    let code = code.trim().to_ascii_uppercase();
    let explanation = match code.as_str() {
        "E001" => {
            "\
E001: Key not found

A field access used a key that does not exist in the dict. For example,
  { a: 1 }.b
will produce E001 because the key 'b' is not in the dict.

When a similar key exists (within an edit distance threshold), the error
message includes a 'did you mean' suggestion. When no close match is found,
it lists up to five available keys.

Fix: check the key name for typos, or access the key conditionally with
$get or a default pattern."
        }

        "E002" => {
            "\
E002: Undefined variable

A variable reference (e.g. $x) could not be resolved in any enclosing
scope. This usually means the variable was not bound in any enclosing dict,
let expression, or function parameter, or was referenced before its binding
in a non-letrec context.

Fix: define the variable in an enclosing scope, or check for a typo in the
variable name."
        }

        "E010" => {
            "\
E010: Type mismatch

An operation received a value of the wrong type. For example, adding a
string to an integer, or calling a string as a function.

The error message shows the expected type and the type that was found.

Fix: convert the value to the expected type (e.g. $to-string, $to-int) or
ensure the input has the correct type."
        }

        "E011" => {
            "\
E011: Type assertion failed

A runtime type assertion written as [@Type value] evaluated the value and
found a type different from the annotated type.

Fix: ensure the value actually produces the declared type, or update the
type annotation to match the runtime type."
        }

        "E020" => {
            "\
E020: Arity mismatch

A function was called with the wrong number of positional arguments. For
example, calling a one-argument function with two arguments.

The error message shows how many arguments were expected and how many were
passed.

Fix: supply the correct number of arguments, or update the function
definition if the arity is intentionally changing."
        }

        "E021" => {
            "\
E021: Named argument conflict

A function parameter received both a positional argument and a named
argument in the same call. Only one form can supply a parameter.

Fix: pass the argument either positionally or as a named argument, not both."
        }

        "E022" => {
            "\
E022: Unknown named argument

A named argument was passed to a function that does not declare a parameter
with that name. The error message lists the valid parameter names.

Fix: check the parameter name for typos, or remove the unknown named
argument."
        }

        "E023" => {
            "\
E023: Named argument rejected

A built-in function received a named argument but does not accept named
arguments (built-ins take only positional arguments unless documented
otherwise).

Fix: pass the argument positionally."
        }

        "E024" => {
            "\
E024: Missing required parameter

A function was called without supplying a value for a required parameter
(one without a default). The error message names the missing parameter.

Fix: supply the missing argument positionally or as a named argument."
        }

        "E030" => {
            "\
E030: Duplicate key

A dict literal contained the same key more than once. LLT does not allow
duplicate keys in dict literals; the second definition would silently
shadow the first.

Fix: remove the duplicate key, or merge the values explicitly."
        }

        "E031" => {
            "\
E031: Division by zero

An integer or float division (or modulo) operation had a zero divisor.

Fix: guard the divisor with an if expression, or ensure the denominator
is never zero."
        }

        "E032" => {
            "\
E032: Integer overflow

An arithmetic operation on integers produced a result outside the i64
range (-9223372036854775808 to 9223372036854775807).

Fix: use float arithmetic if the values may be large, or add range checks."
        }

        "E033" => {
            "\
E033: Float not finite

An operation produced or received a non-finite float value (NaN, Infinity,
or -Infinity). LLT does not allow non-finite floats in contexts requiring
well-defined numeric values.

Fix: add guards for division by zero and for inputs that might be NaN or
infinite. Use $is-finite to check a float before converting or comparing."
        }

        "E034" => {
            "\
E034: Empty collection

An operation that requires a non-empty collection (such as $head, $tail,
$min, or $max) was applied to an empty sequence or string.

Fix: check that the collection is non-empty before applying the operation,
or provide a default with $if."
        }

        "E035" => {
            "\
E035: Value not serializable

A value that cannot be represented in JSON (Function, Builtin, or Proxy)
reached the serialization step.

Fix: ensure all values in the output dict are JSON-compatible (strings,
numbers, booleans, null, lists, and dicts). Functions must be applied to
produce a data value before output."
        }

        "E036" => {
            "\
E036: Float out of range for Int

A float-to-integer conversion ($to-int or similar) was attempted on a
finite float whose value is outside the i64 range.

Fix: check the float value before converting, or use a float output type."
        }

        "E040" => {
            "\
E040: Maximum evaluation depth exceeded

The evaluator exceeded its recursion limit. This usually indicates infinite
or very deep mutual recursion.

Fix: restructure the computation to avoid deep recursion. Use iterative
patterns ($fold, $map) instead of recursive function calls where possible.
If the recursion is intentional but bounded, the limit may be raised with
--depth (if supported)."
        }

        "E041" => {
            "\
E041: Maximum JSON nesting depth exceeded

A $from-json call was given a JSON document nested more deeply than the
allowed limit.

Fix: ensure the JSON input does not have excessive nesting, or pre-process
deeply nested JSON before passing it to tinct."
        }

        "E042" => {
            "\
E042: Filesystem access disabled

A $include call was made but the --no-fs flag was passed on the command
line, disabling all filesystem access.

Fix: remove --no-fs if filesystem access is intended, or provide the
included data through stdin JSON (%) instead."
        }

        "E043" => {
            "\
E043: Resource limit exceeded

An operation exceeded a configured resource limit (such as collection
size or string length).

Fix: reduce the size of the collection or string, or check whether the
limit can be raised for your use case."
        }

        "E050" => {
            "\
E050: Include not available in this context

$include was used in a context where the include subsystem is not
initialised (for example, in a unit test or REPL context that does not
set up a base directory).

Fix: run the file with 'tinct eval' rather than in a context that does not
support $include."
        }

        "E051" => {
            "\
E051: Include I/O error

A $include call could not open or read the target file. The error message
includes the OS-level error detail.

Fix: check that the file path is correct and that the file is readable."
        }

        "E052" => {
            "\
E052: Circular include

A $include call would create a cycle: file A includes file B which (directly
or transitively) includes file A again.

Fix: restructure the files to break the include cycle."
        }

        "E053" => {
            "\
E053: Include parse error

A $include call succeeded in reading the file but the file contains invalid
LLT syntax. The error message includes the parser error detail.

Fix: correct the syntax error in the included file."
        }

        "E054" => {
            "\
E054: Included file too large

The file passed to $include exceeds the 10 MB size limit.

Fix: split the file, or load it through an external pre-processing step
that feeds the data through stdin JSON."
        }

        "E055" => {
            "\
E055: Include integrity hash mismatch

The file passed to $include was successfully read but its blake3 hash does
not match the expected hash supplied as the second argument to $include.

Fix: recompute the expected hash with 'tinct hash <file>' and update the
$include call."
        }

        "E056" => {
            "\
E056: Include integrity hash required

The --require-integrity flag was passed on the command line, but a $include
call was made without supplying an expected hash as the second argument.

Fix: pass the blake3 hash of the included file as the second argument, e.g.
  $include \"config.llt\" \"blake3:abc123...\""
        }

        "E057" => {
            "\
E057: Include path not permitted

A $include call attempted to access a path that is not a descendant of any
directory listed with --allow-path.

Fix: add the required directory to the --allow-path allowlist, or ensure
$include only accesses files within the already-allowed directories."
        }

        "E060" => {
            "\
E060: Parse conversion failed

A $to-int or $to-float call could not parse the supplied string.

Fix: ensure the string is a valid integer or floating-point literal before
converting, or use $try to handle the error gracefully."
        }

        "E061" => {
            "\
E061: Invalid JSON

A $from-json call received a string that is not valid JSON.

Fix: ensure the input to $from-json is a well-formed JSON string."
        }

        "E062" => {
            "\
E062: JSON number out of range

A JSON number in a $from-json call is outside the representable range for
LLT's numeric types.

Fix: pre-process the JSON to reduce large numbers, or represent them as
strings."
        }

        "E070" => {
            "\
E070: Circular dependency

Two or more values in a letrec scope (dict or document) depend on each other
in a cycle, making it impossible to evaluate either. For example:
  { a: $b, b: $a }
cannot be evaluated because 'a' depends on 'b' which depends on 'a'.

Fix: break the cycle by introducing at least one value that does not depend
on the others, or restructure the computation to be non-circular."
        }

        "E080" => {
            "\
E080: User error

The $error built-in was called explicitly in user code. This is an
intentional error raised by the program logic.

Fix: check the conditions under which $error is called and ensure the
calling code supplies valid input."
        }

        "E099" => {
            "\
E099: Internal error

An unexpected internal condition occurred in the evaluator. This should not
happen in normal use.

Fix: file a bug report with the full error message and the LLT source file
that triggered the error."
        }

        _ => {
            return Err(format!(
                "unknown error code: {code}\n\
                 Run 'tinct explain <code>' with a valid code, e.g. E001 through E099.\n\
                 Known codes: E001, E002, E010, E011, E020-E024, E030-E036, \
                 E040-E043, E050-E057, E060-E062, E070, E080, E099."
            ));
        }
    };
    println!("{explanation}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_seconds() {
        assert_eq!(parse_duration("5s"), Ok(5));
        assert_eq!(parse_duration("1s"), Ok(1));
        assert_eq!(parse_duration("30s"), Ok(30));
    }

    #[test]
    fn parse_duration_milliseconds() {
        assert_eq!(parse_duration("500ms"), Ok(1));
        assert_eq!(parse_duration("1000ms"), Ok(1));
        assert_eq!(parse_duration("1500ms"), Ok(2));
        assert_eq!(parse_duration("1ms"), Ok(1));
    }

    #[test]
    fn parse_duration_minutes() {
        assert_eq!(parse_duration("1m"), Ok(1 * 60));
        assert_eq!(parse_duration("2m"), Ok(2 * 60));
    }

    #[test]
    fn parse_duration_bare_number() {
        assert_eq!(parse_duration("10"), Ok(10));
        assert_eq!(parse_duration("1"), Ok(1));
    }

    #[test]
    fn parse_duration_zero_rejected() {
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("0m").is_err());
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("0ms").is_err());
    }

    #[test]
    fn parse_duration_invalid() {
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("").is_err());
        assert!(parse_duration("5x").is_err());
        assert!(parse_duration("-1s").is_err());
    }

    #[test]
    fn parse_duration_whitespace() {
        assert_eq!(parse_duration(" 5s "), Ok(5));
        assert_eq!(parse_duration("  10  "), Ok(10));
    }

    #[test]
    fn parse_duration_999ms_boundary() {
        // 999ms rounds up to 1 second (alarm() requires whole seconds)
        assert_eq!(parse_duration("999ms"), Ok(1));
    }

    #[test]
    fn parse_duration_large_minutes_overflow() {
        // 100000000 * 60 overflows u32::MAX, should return error
        assert!(parse_duration("100000000m").is_err());
        let err_msg = parse_duration("100000000m").unwrap_err();
        assert!(err_msg.contains("duration out of range"));
    }

    #[test]
    fn parse_duration_ms_u32max_overflow() {
        // u32::MAX = 4294967295, so 4294967296 seconds should overflow
        // 4294967296000ms → (4294967296000 + 999) / 1000 = 4294967296 > u32::MAX
        assert!(parse_duration("4294967296000ms").is_err());
        let err_msg = parse_duration("4294967296000ms").unwrap_err();
        assert!(err_msg.contains("duration out of range"));
    }

    #[test]
    fn parse_duration_checked_add_boundary() {
        // Test the exact boundary where checked_add(999) returns None.
        // u64::MAX = 18446744073709551615
        // u64::MAX - 998 = 18446744073709550617
        // When we call checked_add(999), we get: 18446744073709550617 + 999 = 18446744073709551616
        // This exceeds u64::MAX, so checked_add returns None.
        assert!(parse_duration("18446744073709550617ms").is_err());
        let err_msg = parse_duration("18446744073709550617ms").unwrap_err();
        assert!(err_msg.contains("duration out of range"));
    }
}
