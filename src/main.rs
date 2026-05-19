//! LLT command-line tool: parses and evaluates `.llt` files, outputs JSON or LLT display format.

use clap::{Parser, Subcommand, ValueEnum};
use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;
use std::process;
use std::rc::Rc;
use std::str::FromStr;
use tinct::{
    create_stdlib_env, deep_materialize, eval_file_with_input, format_with_json_llt, json_to_value,
    literate, materialize, parse, value_to_json, EvalContext, Span, Thunk, MAX_FILE_SIZE,
};

// Exit codes for llt eval
const EXIT_ERROR: i32 = 1;
const EXIT_TIMEOUT: i32 = 2;
// Note: RLIMIT_AS violations cause SIGSEGV/SIGKILL from the kernel, not a clean exit code.
// RLIMIT_CPU violations cause SIGXCPU (soft) or SIGKILL (hard). Both terminate without EXIT_ERROR.

/// A pipeline stage: either a file path or an inline expression.
#[derive(Debug, Clone)]
enum PipelineStage {
    File(String),
    Expr(String),
}

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
    #[clap(alias = "eval")]
    Run {
        /// Deep-force all thunks before serializing (surfaces errors before partial output).
        #[arg(long)]
        eval: bool,

        /// Disable all filesystem access: suppresses %pwd, %libdir, and any caps injected
        /// via --cap-fs or --cap-file. Scripts that attempt filesystem operations fail.
        /// Use --no-pwd or --no-libdir for fine-grained suppression.
        #[arg(long)]
        no_fs: bool,

        /// Require all $include calls to provide an integrity hash. Hashless includes error.
        #[arg(long)]
        require_integrity: bool,

        /// Type errors are fatal (exit with code 1). Without --strict, type checking is advisory.
        #[arg(long)]
        strict: bool,

        /// Wall-clock timeout (e.g. "5s", "500ms", "2m"). Exit code 2 on expiry.
        #[arg(long)]
        timeout: Option<String>,

        /// Disable Landlock filesystem ACL enforcement.
        /// By default, when --cap-fs is specified on Linux, Landlock is applied as
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

        /// Do not inject `%pwd` DirCap into the root environment.
        /// When set, [open %pwd ...] and [include %pwd ...] fail with undefined variable.
        #[arg(long)]
        no_pwd: bool,

        /// Do not inject `%libdir` DirCap into the root environment.
        /// When set, [include %libdir ...] fails with undefined variable.
        #[arg(long)]
        no_libdir: bool,

        /// Override the standard library directory path.
        /// By default, the stdlib directory is auto-detected relative to the binary location.
        /// Use this flag to specify a custom stdlib directory (e.g., for testing or non-standard layouts).
        #[arg(long, value_name = "PATH")]
        libdir_path: Option<String>,

        /// Inject a named DirCap into the root environment (may be repeated).
        /// Format: NAME=PATH — binds %NAME to a DirCap for PATH.
        /// Example: --cap-fs data=/var/data injects %data as a DirCap for /var/data.
        #[arg(long, value_name = "NAME=PATH")]
        cap_fs: Vec<String>,

        /// Inject a named NetCap into the root environment (may be repeated).
        /// Format: NAME=ENTRY — binds %NAME to a NetCap.
        /// Multiple uses of the same NAME accumulate into one NetCap allowlist.
        /// Example: --cap-net api=api.internal --cap-net api=10.42.0.0/16
        #[arg(long, value_name = "NAME=ENTRY")]
        cap_net: Vec<String>,

        /// Disable the default %clock capability (blocks all time access).
        /// By default, %clock is injected automatically as a real system clock.
        /// Use this flag for sandboxed/reproducible execution contexts.
        #[arg(long)]
        no_cap_clock: bool,

        /// Override the default %clock with a fixed timestamp (for testing).
        /// Format: "RFC3339" — binds %clock to a ClockCap returning the fixed timestamp.
        /// Example: --cap-clock-fixed "2024-01-01T00:00:00Z" injects a fixed %clock.
        #[arg(long, value_name = "RFC3339")]
        cap_clock_fixed: Option<String>,

        /// Inject a named file Handle into the root environment (may be repeated).
        /// Format: NAME=PATH[:MODE] — pre-opens PATH and binds %NAME to a Handle.
        /// MODE: r (readable text), rb (readable binary), w (writable text), wb (writable binary),
        ///       a (appendable text), ab (appendable binary).
        ///       Extended: [Readable Writable ...] (valid: Readable, Writable, Appendable, Binary).
        ///       No :MODE suffix → r (readable text).
        /// Example: --cap-file config=Cargo.toml:r injects %config as a readable Handle.
        /// --no-fs also suppresses --cap-file Handles (filesystem access is blocked entirely).
        #[arg(long, value_name = "NAME=PATH[:MODE]")]
        cap_file: Vec<String>,

        /// Evaluate an inline tinct expression (may be repeated).
        /// Each -e occurrence inserts a pipeline stage at that position in the command line,
        /// interleaved with file arguments. Each expression receives % from the previous stage.
        /// --- is valid inside a single -e string for multiple stages; semicolons are whitespace-equivalent.
        #[arg(short = 'e', long = "expr", value_name = "EXPR")]
        expr: Vec<String>,

        /// Prepend an input formatter from stdlib/cli/in/<format>.llt as the first pipeline stage.
        /// Suppresses stdin JSON auto-detection. Error if the formatter file does not exist.
        #[arg(short = 'i', long = "input", value_name = "FORMAT")]
        input: Option<String>,

        /// Append an output formatter from stdlib/cli/out/<format>.llt as the final pipeline stage.
        /// Error if the formatter file does not exist.
        #[arg(short = 'o', long = "output", value_name = "FORMAT")]
        output: Option<String>,

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

        /// Formatter script to use, resolved from stdlib/cli/fmt/<name>.llt.
        /// Defaults to `pretty`. Use `-o compact` for single-line compact output.
        #[arg(
            short = 'o',
            long = "output",
            value_name = "NAME",
            default_value = "pretty"
        )]
        output: String,

        /// Type errors are fatal (exit with code 1). Without --strict, formatting proceeds regardless of type errors.
        #[arg(long)]
        strict: bool,

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
    /// Describe the input contract of an LLT file.
    ///
    /// Extracts `%@Type` annotations and schema dicts, printing a human-readable
    /// summary of the expected input shape. Use `--json` for machine-readable output.
    Describe {
        /// Emit machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,

        /// Input LLT file to describe.
        file: String,
    },
    /// Show a detailed explanation for an error code (e.g. E001).
    Explain {
        /// Error code to explain (e.g. E001, E010, E070).
        code: String,
    },
    /// Type-check an LLT file without evaluating.
    ///
    /// Runs parse → desugar → macro-expand → typecheck pipeline.
    /// Exits with code 1 if any type errors or warnings are found.
    /// Exits with code 0 on clean file.
    Lint {
        /// File to lint
        file: String,

        /// Disable all filesystem access (default for lint).
        /// Use --cap-fs to allow specific directories for include resolution.
        #[arg(long, default_value_t = true)]
        no_fs: bool,

        /// Inject a named DirCap into the root environment (may be repeated).
        /// Format: NAME=PATH — binds %NAME to a DirCap for PATH.
        /// Example: --cap-fs data=/var/data injects %data as a DirCap for /var/data.
        #[arg(long, value_name = "NAME=PATH")]
        cap_fs: Vec<String>,

        /// Inject a named NetCap into the root environment (may be repeated).
        /// Format: NAME=ENTRY — binds %NAME to a NetCap.
        /// Multiple uses of the same NAME accumulate into one NetCap allowlist.
        #[arg(long, value_name = "NAME=ENTRY")]
        cap_net: Vec<String>,
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
        /// Skip inline marker substitution in weave mode (preserve <!-- tinct-result: ... --> markers).
        #[arg(long)]
        no_substitute: bool,

        /// Type errors are fatal (exit with code 1). Without --strict, type checking is advisory.
        #[arg(long)]
        strict: bool,

        /// Write weaved output back to the source file atomically (weave mode only).
        /// Writes to a .tmp file then renames to the source path.
        #[arg(short, long)]
        in_place: bool,

        /// Compare actual output against expected === sections; exit 1 on mismatch (weave mode only).
        /// Blocks without === sections pass vacuously.
        #[arg(long)]
        verify: bool,

        /// Any evaluation error exits 1 immediately instead of embedding in === error section (weave mode only).
        #[arg(long)]
        fail_on_errors: bool,

        /// Inject a named DirCap into the root environment (may be repeated).
        /// Format: NAME=PATH:MODE — binds %NAME to a DirCap.
        /// MODE: r (read-only), w (read-write), rw (read-write). Default: r.
        /// Example: --cap-fs data=/var/data:r injects %data as a read-only DirCap.
        #[arg(long, value_name = "NAME=PATH:MODE")]
        cap_fs: Vec<String>,

        /// Inject a named NetCap into the root environment (may be repeated).
        /// Format: NAME=HOST:PORT — binds %NAME to a NetCap.
        /// Example: --cap-net api=api.internal:443
        #[arg(long, value_name = "NAME=HOST:PORT")]
        cap_net: Vec<String>,
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

/// Structure to hold actual output for each block in literate mode.
#[derive(Debug)]
struct BlockOutput {
    out: Option<String>,   // JSON result or (emit)
    warn: Option<String>,  // Type warnings
    error: Option<String>, // Error message if evaluation failed
    info: Option<String>,  // Log output (future: from `log` builtin)
}

fn main() {
    let cli = Cli::parse();

    // Raise the process stack soft limit to match the hard limit (or 512MB if
    // the hard limit is higher). This allows large eval thread stacks in debug
    // builds where stdlib Rc<Environment> drop chains require ~100MB+.
    // Without this, pthread ignores stack_size() requests above RLIMIT_STACK.
    #[cfg(unix)]
    unsafe {
        let mut rl = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_STACK, &mut rl) == 0 {
            // Raise soft limit to max(hard_limit, 512MB) without changing hard limit
            let target: u64 = 512 * 1024 * 1024;
            let new_cur = if rl.rlim_max == libc::RLIM_INFINITY || rl.rlim_max >= target {
                target
            } else {
                rl.rlim_max
            };
            let new_rl = libc::rlimit {
                rlim_cur: new_cur,
                rlim_max: rl.rlim_max,
            };
            let _ = libc::setrlimit(libc::RLIMIT_STACK, &new_rl);
        }
    }

    // Materialize is iterative (materialize_rc loop); no large worker stack needed.
    // The REPL spawns its own 128MB thread for eval when needed (src/repl.rs).
    let result = match cli.command {
        Commands::Run {
            eval,
            no_fs,
            require_integrity,
            strict,
            timeout,
            no_landlock,
            max_memory,
            max_cpu,
            max_fds,
            no_env,
            allow_env,
            no_pwd,
            no_libdir,
            libdir_path,
            cap_fs,
            cap_net,
            no_cap_clock,
            cap_clock_fixed,
            cap_file,
            expr,
            input,
            output,
            files,
        } => run_eval(
            &files,
            eval,
            no_fs,
            require_integrity,
            strict,
            timeout.as_deref(),
            no_landlock,
            max_memory,
            max_cpu,
            max_fds,
            no_env,
            allow_env,
            no_pwd,
            no_libdir,
            libdir_path,
            cap_fs,
            cap_net,
            no_cap_clock,
            cap_clock_fixed,
            cap_file,
            expr,
            input,
            output,
        ),
        Commands::Hash { file } => run_hash(&file),
        Commands::Fmt {
            check,
            in_place,
            output,
            strict,
            file,
        } => run_fmt(&file, check, in_place, &output, strict),
        #[cfg(feature = "repl")]
        Commands::Repl => tinct::repl::run_repl(),
        #[cfg(feature = "lsp")]
        Commands::Lsp => tinct::lsp::run_lsp().map_err(|e| format!("{e}")),
        Commands::Describe { json, file } => run_describe(&file, json),
        Commands::Explain { code } => run_explain(&code),
        Commands::Lint {
            file,
            no_fs,
            cap_fs,
            cap_net,
        } => run_lint(&file, no_fs, &cap_fs, &cap_net),
        Commands::Literate {
            mode,
            file,
            no_substitute,
            strict,
            in_place,
            verify,
            fail_on_errors,
            cap_fs,
            cap_net,
        } => run_literate(
            &file,
            &mode,
            no_substitute,
            strict,
            in_place,
            verify,
            fail_on_errors,
            &cap_fs,
            &cap_net,
        ),
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
        sa.sa_sigaction = timeout_handler as *const () as libc::sighandler_t;
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
fn setup_seccomp(allow_network: bool) -> Result<(), String> {
    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter, TargetArch};
    use std::collections::BTreeMap;

    // Determine the target architecture for the BPF filter.
    // seccompiler requires this to match the running kernel's architecture.
    let arch = std::cfg_select! {
        target_arch = "x86_64" => TargetArch::x86_64,
        target_arch = "aarch64" => TargetArch::aarch64,
        _ => {
            // seccompiler only supports x86_64 and aarch64. On other architectures
            // (e.g. arm, riscv64, s390x) we degrade gracefully without error.
            return Ok(())
        }
    };

    // Build the syscall blocklist. Each entry maps a syscall number to an empty
    // rule vector, which means "match unconditionally" — the match_action
    // (EPERM) fires for every invocation of that syscall.
    let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();

    // Network syscalls — block all network socket operations unless --cap-net
    // is present (allow_network=true), which grants explicit network authority.
    if !allow_network {
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
/// Auto-triggered when `--cap-fs` entries are present (unless `--no-landlock` is set).
/// The Landlock LSM restricts the process to read-only access on the `--cap-fs` paths.
/// If the kernel doesn't support Landlock (< 5.13, or disabled), this returns `Ok(()`.
/// Cap-std RESOLVE_BENEATH remains the primary enforcement; Landlock is defense-in-depth.
///
/// Landlock catches bugs: if a bug in cap-std or DirCap handling allows an unauthorized
/// path to reach `open()`, Landlock blocks it at the kernel level.
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
/// `allowed_paths` — directories from `--cap-fs` entries (Landlock roots).
/// `extra_readable` — additional directories that must be readable for the process to
///   function (e.g., the directories containing the main input files). These are NOT
///   part of the --cap-fs allowlist; they only let the OS read the primary files.
fn setup_landlock(allowed_paths: &[PathBuf], extra_readable: &[PathBuf]) -> Result<(), String> {
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
    };

    // V3 corresponds to Linux 5.19+. The crate gracefully degrades to a lower ABI
    // version if the running kernel doesn't support V3 (best-effort restriction).
    let abi = ABI::V3;

    // Build the initial ruleset handling both read and write filesystem access.
    // Cap-fs paths get read+write; extra_readable paths get read-only.
    let mut ruleset_created = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| format!("landlock: failed to configure ruleset: {e}"))?
        .create()
        .map_err(|e| format!("landlock: failed to create ruleset: {e}"))?;

    // Grant read+write access to --cap-fs paths (full capability).
    for path in allowed_paths {
        if !path.exists() {
            continue;
        }
        let fd = PathFd::new(path).map_err(|e| {
            format!(
                "landlock: cannot open allowed path \"{}\": {e}",
                path.display()
            )
        })?;
        let rule = PathBeneath::new(fd, AccessFs::from_all(abi));
        ruleset_created = ruleset_created.add_rule(rule).map_err(|e| {
            format!(
                "landlock: failed to add rule for \"{}\": {e}",
                path.display()
            )
        })?;
    }

    // Grant read-only access to extra_readable paths (input files, libdir).
    for path in extra_readable {
        if !path.exists() {
            continue;
        }
        let fd = PathFd::new(path).map_err(|e| {
            format!(
                "landlock: cannot open readable path \"{}\": {e}",
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
/// Wrapper around `tinct::find_libdir_path()`.
fn find_libdir_path() -> Option<std::path::PathBuf> {
    tinct::find_libdir_path()
}

/// Resolve the stdlib directory path, with optional CLI override.
///
/// If `override_path` is Some, use it; otherwise fall back to auto-detection.
fn resolve_libdir_path(override_path: Option<&str>) -> Option<std::path::PathBuf> {
    match override_path {
        Some(path) => Some(std::path::PathBuf::from(path)),
        None => find_libdir_path(),
    }
}

/// Parse a CLI NetCap entry (from --cap-net NAME=ENTRY).
///
/// Special value: `any` creates an unrestricted NetCap that allows all hosts/ports.
/// This is the CLI equivalent of a wildcard allowlist, intended for trusted scripts.
fn parse_cli_net_cap_entry(s: &str) -> Result<tinct::NetCapEntry, String> {
    use tinct::NetCapEntry;

    if s == "any" {
        return Ok(NetCapEntry::Any);
    }

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
        // CIDR range
        match s.parse::<ipnet::IpNet>() {
            Ok(net) => Ok(NetCapEntry::Cidr(net)),
            Err(e) => Err(format!("--cap-net: invalid CIDR notation '{}': {}", s, e)),
        }
    } else {
        // Plain hostname
        Ok(NetCapEntry::Hostname(s.to_string()))
    }
}

/// Reconstruct the interleaved order of files and -e expressions from raw CLI args.
/// Clap doesn't preserve the relative order of positional args and flags, so we
/// parse std::env::args_os() to determine which files and -e expressions appeared
/// in what order after the "run" subcommand.
fn interleave_files_and_exprs(files: &[String], exprs: &[String]) -> Vec<PipelineStage> {
    let mut result = Vec::new();

    // Parse raw args to find the order. We need to track which files and exprs
    // we've consumed from the clap-parsed vectors.
    let mut file_iter = files.iter();
    let mut expr_iter = exprs.iter();

    let mut args = std::env::args_os().skip(1); // Skip program name
    let mut seen_run = false;

    while let Some(arg) = args.next() {
        let arg_str = arg.to_string_lossy();

        // Skip until we see "run" subcommand
        if !seen_run {
            if arg_str == "run" {
                seen_run = true;
            }
            continue;
        }

        // Check if this is a -e/--expr flag
        if arg_str == "-e" || arg_str == "--expr" {
            // Next arg is the expression value
            if let Some(_expr_val) = args.next() {
                // Match against the next unconsumed expr from clap's list
                if let Some(expr) = expr_iter.next() {
                    result.push(PipelineStage::Expr(expr.clone()));
                }
            }
        } else if arg_str.starts_with("--expr=") {
            // Handle --expr=value form
            // Just consume the next expr from the iterator (clap already parsed it)
            if let Some(expr) = expr_iter.next() {
                result.push(PipelineStage::Expr(expr.clone()));
            }
        } else if arg_str.starts_with('-') {
            // Skip other flags and their values
            // This is a heuristic: if it starts with -, it's a flag.
            // Some flags take values, some don't. We'll handle common ones.
            let flag = arg_str.as_ref();
            if matches!(
                flag,
                "-i" | "--input"
                    | "-o"
                    | "--output"
                    | "--timeout"
                    | "--allow-path"
                    | "--max-memory"
                    | "--max-cpu"
                    | "--max-fds"
                    | "--allow-env"
                    | "--cap-fs"
                    | "--cap-net"
                    | "--cap-clock"
                    | "--cap-clock-fixed"
                    | "--cap-file"
            ) {
                // These flags take a value, skip it
                args.next();
            }
            // Boolean flags like --eval, --no-fs, --strict, etc. don't take values
        } else {
            // This is a positional argument (file)
            // Match against the next unconsumed file from clap's list
            if let Some(file) = file_iter.next() {
                result.push(PipelineStage::File(file.clone()));
            }
        }
    }

    // Append any remaining files (shouldn't happen in correct usage)
    for file in file_iter {
        result.push(PipelineStage::File(file.clone()));
    }

    // Append any remaining exprs (shouldn't happen in correct usage)
    for expr in expr_iter {
        result.push(PipelineStage::Expr(expr.clone()));
    }

    result
}

fn run_eval(
    file_paths: &[String],
    force_eval: bool,
    no_fs: bool,
    require_integrity: bool,
    strict: bool,
    timeout: Option<&str>,
    no_landlock: bool,
    max_memory: Option<u64>,
    max_cpu: Option<u64>,
    max_fds: Option<u64>,
    no_env: bool,
    allow_env: Vec<String>,
    no_pwd: bool,
    no_libdir: bool,
    libdir_path: Option<String>,
    cap_fs: Vec<String>,
    cap_net: Vec<String>,
    no_cap_clock: bool,
    cap_clock_fixed: Option<String>,
    cap_file: Vec<String>,
    expr: Vec<String>,
    input: Option<String>,
    output: Option<String>,
) -> Result<(), String> {
    // Build the complete pipeline: [input formatter] + [files/exprs interleaved] + [output formatter]
    let mut pipeline_stages: Vec<PipelineStage> = Vec::new();

    // Prepend -i input formatter if specified
    if let Some(ref input_format) = input {
        let libdir_path = resolve_libdir_path(libdir_path.as_deref())
            .ok_or_else(|| format!("--input: stdlib directory not found (libdir)"))?;
        let input_path = libdir_path
            .join("cli")
            .join("in")
            .join(format!("{}.llt", input_format));
        if !input_path.exists() {
            return Err(format!(
                "--input: formatter not found: {}",
                input_path.display()
            ));
        }
        pipeline_stages.push(PipelineStage::File(
            input_path.to_str().unwrap().to_string(),
        ));
    }

    // Interleave files and -e expressions in the order they appear on the CLI.
    // Clap doesn't preserve mixed positional/flag order, so we reconstruct it
    // by parsing raw CLI arguments.
    let interleaved_stages = interleave_files_and_exprs(file_paths, &expr);
    pipeline_stages.extend(interleaved_stages);

    // Append -o output formatter if specified
    if let Some(ref output_format) = output {
        let libdir_path = resolve_libdir_path(libdir_path.as_deref())
            .ok_or_else(|| format!("--output: stdlib directory not found (libdir)"))?;
        let output_path = libdir_path
            .join("cli")
            .join("out")
            .join(format!("{}.llt", output_format));
        if !output_path.exists() {
            return Err(format!(
                "--output: formatter not found: {}",
                output_path.display()
            ));
        }
        pipeline_stages.push(PipelineStage::File(
            output_path.to_str().unwrap().to_string(),
        ));
    }

    if pipeline_stages.is_empty() {
        return Err("no input files or expressions specified".to_string());
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

    // Check for piped stdin JSON.
    // Suppressed when -i/--input is present (the input formatter reads from stdin Handle).
    // Also suppressed when the first pipeline stage is stdin itself ("-").
    // Returns raw serde_json::Value; conversion to LLT happens after the first
    // EvalContext is created so ThunkIds are allocated in the shared arena.
    let stdin_json = if input.is_none()
        && !pipeline_stages.is_empty()
        && !matches!(pipeline_stages[0], PipelineStage::File(ref p) if p == "-")
    {
        read_stdin_json()?
    } else {
        None
    };

    // Create stdlib environment
    let env = create_stdlib_env().map_err(|e| format!("{e}"))?;

    // Apply Landlock filesystem ACL enforcement (Linux only, defense-in-depth).
    // Auto-triggered when --cap-fs entries are present (unless --no-landlock is set).
    // Derives Landlock roots from the --cap-fs directory paths.
    //
    // Also grant read access to the directories containing the main input files so
    // they can be read before evaluation starts. These extra-readable dirs are NOT
    // part of the --cap-fs allowlist; they're just for reading the primary input files.
    #[cfg(target_os = "linux")]
    if !no_landlock && !cap_fs.is_empty() {
        // Extract directory paths from --cap-fs NAME=PATH[:MODE] entries
        // Strip the :MODE suffix if present (same logic as DirCap parsing)
        let cap_fs_paths: Vec<PathBuf> = cap_fs
            .iter()
            .filter_map(|entry| {
                entry.split_once('=').map(|(_, path_and_mode)| {
                    // Split PATH:MODE on the last colon (rsplit_once handles Windows drive letters)
                    let (path_str, _mode_str) = match path_and_mode.rsplit_once(':') {
                        Some((path, mode)) => (path, Some(mode)),
                        None => (path_and_mode, None),
                    };
                    PathBuf::from(path_str.trim())
                })
            })
            .collect();

        // Collect the canonical parent directories of each input file.
        // Inline expressions (PipelineStage::Expr) don't need extra_readable paths.
        let mut extra_readable: Vec<PathBuf> = pipeline_stages
            .iter()
            .filter_map(|stage| match stage {
                PipelineStage::File(p) if p != "-" => Some(p.as_str()),
                _ => None,
            })
            .filter_map(|p| {
                let path = std::path::Path::new(p);
                let dir = match path.parent().filter(|d| !d.as_os_str().is_empty()) {
                    Some(d) => d.to_path_buf(),
                    None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                };
                dir.canonicalize().ok()
            })
            .collect();

        // Include the stdlib directory in Landlock-readable paths so that
        // `[include %libdir ...]` works when Landlock is active.
        if !no_libdir {
            if let Some(libdir) = resolve_libdir_path(libdir_path.as_deref()) {
                if let Ok(canon) = libdir.canonicalize() {
                    extra_readable.push(canon);
                }
            }
        }
        setup_landlock(&cap_fs_paths, &extra_readable)?;
    }
    // On non-Linux platforms, --no-landlock is accepted for CLI compatibility
    // but has no effect (Landlock is a Linux-only API).
    #[cfg(not(target_os = "linux"))]
    let _ = no_landlock;

    // Install seccomp-bpf network and process sandbox (Linux only).
    // Applied after Landlock so that both kernel-level defenses are active before
    // eval. Gracefully degrades on unsupported kernels (prints warning, continues).
    // Network syscalls are allowed when --cap-net is present (explicit network authority).
    let allow_network = !cap_net.is_empty();
    #[cfg(target_os = "linux")]
    if let Err(e) = setup_seccomp(allow_network) {
        eprintln!("warning: seccomp sandbox not active: {e}");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = allow_network;

    // Inject `%pwd` DirCap into the root environment (unless --no-pwd is set).
    // `%pwd` points to the directory of the first input file (or the process cwd when
    // evaluating stdin/inline expressions). This allows `[include %pwd "sibling.llt"]`
    // to resolve relative to the evaluated file, which is the most natural behavior.
    // --no-pwd enforcement: when the flag is set, `%pwd` is NOT injected, so
    // any reference to `%pwd` in the program will fail with "undefined variable".
    if !no_pwd {
        use tinct::Value;
        // Determine %pwd: file's parent dir if a file is given, otherwise cwd.
        // Use canonicalize() so symlinks are resolved; fall back to cwd if the
        // parent directory doesn't exist (e.g., the file itself is missing —
        // the open error will be reported shortly with the correct message).
        let pwd_path = {
            let first_file = file_paths.iter().find(|p| p.as_str() != "-");
            match first_file {
                Some(fp) => {
                    let p = std::path::Path::new(fp);
                    let parent = p.parent().filter(|d| !d.as_os_str().is_empty());
                    match parent {
                        Some(dir) => dir.canonicalize().unwrap_or_else(|_| {
                            // Parent doesn't exist yet (e.g., file is missing). Fall back to
                            // cwd; the file-not-found error will be reported shortly.
                            std::env::current_dir()
                                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                        }),
                        None => std::env::current_dir().map_err(|e| {
                            format!("cannot determine working directory for %pwd: {e}")
                        })?,
                    }
                }
                None => std::env::current_dir()
                    .map_err(|e| format!("cannot determine working directory for %pwd: {e}"))?,
            }
        };
        let pwd_dir = cap_std::fs::Dir::open_ambient_dir(&pwd_path, cap_std::ambient_authority())
            .map_err(|e| format!("cannot open %pwd directory: {e}"))?;
        let pwd_value = Value::DirCap {
            dir: Rc::new(pwd_dir),
            perms: tinct::DirPerms::full(),
        };
        let pwd_thunk = tinct::Thunk::new_materialized(pwd_value, tinct::Span::origin());
        env.borrow_mut()
            .insert("%pwd".to_string(), Rc::new(pwd_thunk));
    }

    // Inject `%stdin` Handle for fd 0 into the root environment only when `-i` is present.
    // When `-i` is not present, stdin is read for JSON auto-detection instead.
    if input.is_some() {
        use std::cell::RefCell;
        use std::collections::HashMap;
        use std::io::BufReader;
        use tinct::Value;

        // Create stdin handle with default caps
        let mut caps = HashMap::new();
        caps.insert("Readable".to_string(), Value::Bool(true));
        caps.insert("Text".to_string(), Value::Bool(true));

        let stdin_handle = Value::Handle {
            caps,
            inner: Rc::new(RefCell::new(
                Box::new(BufReader::new(std::io::stdin())) as Box<dyn std::io::BufRead>
            )),
            write_inner: None,
            seek_inner: None,
            raw_tcp: None,
            creation_span: tinct::Span::origin(),
        };
        let stdin_thunk = tinct::Thunk::new_materialized(stdin_handle, tinct::Span::origin());
        env.borrow_mut()
            .insert("%stdin".to_string(), Rc::new(stdin_thunk));
    }

    // Inject `%libdir` DirCap for the stdlib directory (unless --no-libdir is set).
    // --no-libdir enforcement: when the flag is set, `%libdir` is NOT injected, so
    // any reference to `%libdir` in the program will fail with "undefined variable".
    // Phase 1: resolve %libdir from the binary's location, --libdir-path override, or a well-known relative path.
    // If resolution fails, %libdir is not injected (stdlib is embedded at compile time anyway).
    // The resolved path is also saved for the JSON output path (format_with_json_llt).
    let resolved_libdir_path: Option<std::path::PathBuf> = if !no_libdir {
        resolve_libdir_path(libdir_path.as_deref())
    } else {
        None
    };
    if !no_libdir {
        use tinct::Value;
        if let Some(ref path) = resolved_libdir_path {
            if let Ok(libdir_std) =
                cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())
            {
                let libdir_value = Value::DirCap {
                    dir: Rc::new(libdir_std),
                    perms: tinct::DirPerms::full(),
                };
                let libdir_thunk =
                    tinct::Thunk::new_materialized(libdir_value, tinct::Span::origin());
                env.borrow_mut()
                    .insert("%libdir".to_string(), Rc::new(libdir_thunk));
            }
            // If the dir can't be opened, silently skip — stdlib is embedded anyway.
        }
        // TODO(io-phase2): --libdir-path PATH override for custom installations
    }

    // Inject --cap-fs NAME=PATH[:MODE] entries into the root environment as `%NAME`.
    // The `%` prefix makes injected caps visually distinct from user-defined variables.
    // MODE syntax: r/w/a/s/l letters or [Cap1 Cap2 ...] extended form.
    {
        use tinct::{DirPerms, Value};
        for cap_fs_entry in &cap_fs {
            let (name, path_and_mode) = cap_fs_entry.split_once('=').ok_or_else(|| {
                format!(
                    "--cap-fs: expected NAME=PATH[:MODE] format, got {:?}",
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

            // Split PATH:MODE on the last colon (rsplit_once handles Windows drive letters)
            let (path_str, mode_str) = match path_and_mode.rsplit_once(':') {
                Some((path, mode)) => (path, Some(mode)),
                None => (path_and_mode, None),
            };

            let cap_path = std::path::Path::new(path_str.trim());
            let cap_dir =
                cap_std::fs::Dir::open_ambient_dir(cap_path, cap_std::ambient_authority())
                    .map_err(|e| {
                        format!(
                            "--cap-fs: cannot open directory {:?}: {e}",
                            cap_path.display()
                        )
                    })?;

            // Parse mode into DirPerms
            let perms = if let Some(mode) = mode_str {
                let mode = mode.trim();
                if mode.starts_with('[') {
                    // Extended syntax: [Readable Writable ...]
                    if !mode.ends_with(']') {
                        return Err(format!(
                            "--cap-fs: extended mode must end with ']', got {:?}",
                            mode
                        ));
                    }
                    let caps_str = &mode[1..mode.len() - 1];
                    let mut perms = DirPerms {
                        readable: false,
                        statable: false,
                        listable: false,
                        writable: false,
                        appendable: false,
                        deletable: false,
                        renameable: false,
                    };
                    for cap_name in caps_str.split_whitespace() {
                        match cap_name {
                            "Readable" => perms.readable = true,
                            "Statable" => perms.statable = true,
                            "Listable" => perms.listable = true,
                            "Writable" => perms.writable = true,
                            "Appendable" => perms.appendable = true,
                            "Deletable" => perms.deletable = true,
                            "Renameable" => perms.renameable = true,
                            _ => {
                                return Err(format!(
                                    "--cap-fs: unknown capability {:?} in extended mode",
                                    cap_name
                                ))
                            }
                        }
                    }
                    perms
                } else {
                    // Letter mode: r/w/a/s/l
                    let mut perms = DirPerms {
                        readable: false,
                        statable: false,
                        listable: false,
                        writable: false,
                        appendable: false,
                        deletable: false,
                        renameable: false,
                    };
                    for c in mode.chars() {
                        if let Some(letter_perms) = DirPerms::from_letter(c) {
                            perms = perms.union(&letter_perms);
                        } else {
                            return Err(format!(
                                "--cap-fs: unknown mode letter {:?} (expected r/w/a/s/l)",
                                c
                            ));
                        }
                    }
                    perms
                }
            } else {
                // No mode specified → full access
                DirPerms::full()
            };

            let cap_value = Value::DirCap {
                dir: Rc::new(cap_dir),
                perms,
            };
            let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
            // Inject as `%NAME` (auto-prefix %).
            let scoped_name = if name.starts_with('%') {
                name.to_string()
            } else {
                format!("%{name}")
            };
            env.borrow_mut().insert(scoped_name, Rc::new(cap_thunk));
        }
    }

    // Inject --cap-net NAME=ENTRY entries into the root environment as `%NAME`.
    // Multiple uses of the same NAME accumulate into one NetCap allowlist.
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

            // Parse the entry using parse_cli_net_cap_entry.
            // Key is stored with % prefix so accumulation works correctly.
            let entry = parse_cli_net_cap_entry(entry_str)?;
            let scoped_name = if name.starts_with('%') {
                name.to_string()
            } else {
                format!("%{name}")
            };
            net_caps.entry(scoped_name).or_default().push(entry);
        }

        // Create NetCap values and inject them as `%NAME`.
        for (name, entries) in net_caps {
            let cap_value = Value::NetCap(Rc::new(entries));
            let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
            env.borrow_mut().insert(name, Rc::new(cap_thunk));
        }
    }

    // Inject %clock into the root environment (unless --no-cap-clock is set).
    // Default: real system clock.
    // --cap-clock-fixed "RFC3339": override with fixed timestamp.
    if !no_cap_clock {
        use tinct::{ClockCapInner, Value};

        let cap_value = if let Some(timestamp_str) = &cap_clock_fixed {
            // Parse the RFC 3339 timestamp using jiff
            let timestamp = jiff::Timestamp::from_str(timestamp_str).map_err(|e| {
                format!(
                    "--cap-clock-fixed: invalid RFC 3339 timestamp '{}': {}",
                    timestamp_str, e
                )
            })?;
            // Convert to nanoseconds (i64)
            let nanos = i64::try_from(timestamp.as_nanosecond()).map_err(|_| {
                format!(
                    "--cap-clock-fixed: timestamp '{}' is out of i64 range",
                    timestamp_str
                )
            })?;
            Value::ClockCap(Rc::new(ClockCapInner::Fixed(nanos)))
        } else {
            // Default: real system clock
            Value::ClockCap(Rc::new(ClockCapInner::Real))
        };

        let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
        env.borrow_mut()
            .insert("%clock".to_string(), Rc::new(cap_thunk));
    }

    // Inject --cap-file NAME=PATH[:MODE] entries into the root environment as `%NAME`.
    // --no-fs suppresses all cap-file entries (filesystem access is blocked globally).
    if !no_fs {
        use std::collections::HashMap;
        use std::io::{BufReader, BufWriter};

        for cap_file_entry in &cap_file {
            // Parse NAME=PATH[:MODE]
            let (name, rest) = cap_file_entry.split_once('=').ok_or_else(|| {
                format!(
                    "--cap-file: expected NAME=PATH[:MODE] format, got {:?}",
                    cap_file_entry
                )
            })?;
            let name = name.trim();
            if name.is_empty() {
                return Err(format!(
                    "--cap-file: NAME must not be empty in {:?}",
                    cap_file_entry
                ));
            }

            // Split PATH:MODE — mode is the suffix after the last ':'
            // PATH may contain ':' on Windows (e.g. C:\foo.txt); find the last ':' for the mode.
            // If no ':', mode defaults to "rw" (read-write text).
            let (path_str, mode_str) = match rest.rsplit_once(':') {
                Some((path, mode)) => (path.trim(), Some(mode.trim())),
                None => (rest.trim(), None),
            };

            if path_str.is_empty() {
                return Err(format!(
                    "--cap-file: PATH must not be empty in {:?}",
                    cap_file_entry
                ));
            }

            // Parse mode into capability flags
            let (readable, writable, appendable, binary) = if let Some(mode) = mode_str {
                if mode.starts_with('[') {
                    // Extended syntax: [Readable Writable ...]
                    if !mode.ends_with(']') {
                        return Err(format!(
                            "--cap-file: extended mode must end with ']', got {:?}",
                            mode
                        ));
                    }
                    let caps_str = &mode[1..mode.len() - 1];
                    let mut readable = false;
                    let mut writable = false;
                    let mut appendable = false;
                    let mut binary = false;
                    for cap_name in caps_str.split_whitespace() {
                        match cap_name {
                            "Readable" => readable = true,
                            "Writable" => writable = true,
                            "Appendable" => appendable = true,
                            "Binary" => binary = true,
                            _ => {
                                return Err(format!(
                                    "--cap-file: unknown capability {:?} in extended mode (expected Readable, Writable, Appendable, Binary)",
                                    cap_name
                                ))
                            }
                        }
                    }
                    (readable, writable, appendable, binary)
                } else {
                    // Letter mode: r/rb/w/wb/a/ab/rw
                    match mode {
                        "r" => (true, false, false, false),
                        "rb" => (true, false, false, true),
                        "w" => (false, true, false, false),
                        "wb" => (false, true, false, true),
                        "a" => (false, false, true, false),
                        "ab" => (false, false, true, true),
                        other => {
                            return Err(format!(
                                "--cap-file: invalid mode {:?} in {:?}: must be r, rb, w, wb, a, ab, or [Readable Writable ...]",
                                other, cap_file_entry
                            ));
                        }
                    }
                }
            } else {
                // No mode specified → r (readable text)
                (true, false, false, false)
            };

            // Validate: at least one of readable/writable/appendable must be set
            if !readable && !writable && !appendable {
                return Err(format!(
                    "--cap-file: mode must specify at least one of Readable, Writable, or Appendable in {:?}",
                    cap_file_entry
                ));
            }

            // Build the Handle or WriteHandle value
            let cap_value = if readable && !writable && !appendable {
                // Read-only: use Handle with read inner
                let file = std::fs::File::open(path_str).map_err(|e| {
                    format!("--cap-file: cannot open {:?} for reading: {e}", path_str)
                })?;
                let buf_reader: Box<dyn std::io::BufRead> = Box::new(BufReader::new(file));
                let mut caps: HashMap<String, tinct::Value> = HashMap::new();
                caps.insert(
                    "Readable".to_string(),
                    tinct::Value::Dict(indexmap::IndexMap::new()),
                );
                if binary {
                    caps.insert(
                        "Binary".to_string(),
                        tinct::Value::Dict(indexmap::IndexMap::new()),
                    );
                } else {
                    caps.insert(
                        "Text".to_string(),
                        tinct::Value::Dict(indexmap::IndexMap::new()),
                    );
                }
                tinct::Value::Handle {
                    caps,
                    inner: Rc::new(std::cell::RefCell::new(buf_reader)),
                    write_inner: None,
                    seek_inner: None,
                    raw_tcp: None,
                    creation_span: tinct::Span::origin(),
                }
            } else if writable && !readable && !appendable {
                // Write-only (truncate): use WriteHandle
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path_str)
                    .map_err(|e| {
                        format!("--cap-file: cannot open {:?} for writing: {e}", path_str)
                    })?;
                let buf_writer: Box<dyn std::io::Write> = Box::new(BufWriter::new(file));
                let mut caps: HashMap<String, tinct::Value> = HashMap::new();
                caps.insert(
                    "Writable".to_string(),
                    tinct::Value::Dict(indexmap::IndexMap::new()),
                );
                if binary {
                    caps.insert(
                        "Binary".to_string(),
                        tinct::Value::Dict(indexmap::IndexMap::new()),
                    );
                } else {
                    caps.insert(
                        "Text".to_string(),
                        tinct::Value::Dict(indexmap::IndexMap::new()),
                    );
                }
                tinct::Value::WriteHandle {
                    caps,
                    inner: Rc::new(std::cell::RefCell::new(buf_writer)),
                }
            } else if appendable && !readable && !writable {
                // Append-only: use WriteHandle with append flag
                let file = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(path_str)
                    .map_err(|e| {
                        format!("--cap-file: cannot open {:?} for appending: {e}", path_str)
                    })?;
                let buf_writer: Box<dyn std::io::Write> = Box::new(BufWriter::new(file));
                let mut caps: HashMap<String, tinct::Value> = HashMap::new();
                caps.insert(
                    "Appendable".to_string(),
                    tinct::Value::Dict(indexmap::IndexMap::new()),
                );
                if binary {
                    caps.insert(
                        "Binary".to_string(),
                        tinct::Value::Dict(indexmap::IndexMap::new()),
                    );
                } else {
                    caps.insert(
                        "Text".to_string(),
                        tinct::Value::Dict(indexmap::IndexMap::new()),
                    );
                }
                tinct::Value::WriteHandle {
                    caps,
                    inner: Rc::new(std::cell::RefCell::new(buf_writer)),
                }
            } else {
                // Read-write or other combinations not yet supported
                return Err(format!(
                    "--cap-file: read-write and multi-capability modes not yet implemented in {:?} (use separate read/write Handles)",
                    cap_file_entry
                ));
            };

            let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
            // Inject as `%NAME` (auto-prefix %).
            let scoped_name = if name.starts_with('%') {
                name.to_string()
            } else {
                format!("%{name}")
            };
            env.borrow_mut().insert(scoped_name, Rc::new(cap_thunk));
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

    // Multi-stage pipeline: process each stage in sequence, passing output as input to the next.
    //
    // ARENA SHARING INVARIANT: All stages in the pipeline must share the same ThunkArena so
    // that ThunkIds allocated by earlier stages remain valid when later stages reference them
    // via the `%` pipeline variable. We establish one base EvalContext for the first stage,
    // then use `with_base_dir_and_path` for subsequent stages — this creates a new config
    // (different base_dir) while sharing the same arena, state, and stdlib_env.
    let mut pipeline_input: Option<Rc<Thunk>> = None;

    let mut thunk = None;
    let mut last_source = String::new();
    let mut last_eval_ctx: Option<Rc<EvalContext>> = None;
    let mut base_eval_ctx: Option<Rc<EvalContext>> = None;

    for stage in &pipeline_stages {
        // Read the LLT source (from file or inline expression)
        let source = match stage {
            PipelineStage::File(file_path) => read_source(file_path)?,
            PipelineStage::Expr(expression) => expression.clone(),
        };

        // Parse
        let ast = parse(&source).map(|o| o.file).map_err(|e| {
            if strict {
                // In strict mode, use rich diagnostic formatting
                let file_name = match stage {
                    PipelineStage::File(fp) => fp.as_str(),
                    PipelineStage::Expr(_) => "<expr>",
                };
                tinct::format_parse_error(&e, &source, file_name)
            } else {
                // Non-strict mode: use simple formatting
                format!("{e}")
            }
        })?;

        // PIPELINE INVARIANT: expand_macros -> desugar -> typecheck -> eval.
        // See also: src/lib.rs (eval_source_with_config pipeline)
        // Expand macros (pre-desugar AST transformation).
        let expand_result = tinct::expand::expand_macros(ast, no_fs).map_err(|e| format!("{e}"))?;
        let mut ast = expand_result.file;
        let _provenance = expand_result.provenance;

        // Desugar $_ implicit lambdas (mandatory pre-typecheck AST transformation).
        tinct::desugar::desugar_file(&mut ast.node);

        // Variable resolution pass (Phase 1 of arena allocation strategy).
        tinct::resolve::resolve_file(&ast.node);

        // Determine base directory for $include resolution (needed for type checking with includes)
        let file_base_dir_path = match stage {
            PipelineStage::Expr(_) => {
                // Inline expressions use cwd as base directory
                std::env::current_dir()
                    .map_err(|e| format!("cannot determine working directory: {e}"))?
            }
            PipelineStage::File(file_path) => {
                if file_path == "-" {
                    std::env::current_dir()
                        .map_err(|e| format!("cannot determine working directory: {e}"))?
                } else {
                    let p = std::path::Path::new(file_path);
                    // Use the file's parent directory; fall back to cwd if the path has no parent
                    // (e.g., a bare filename like "test.llt").
                    match p.parent().filter(|d| !d.as_os_str().is_empty()) {
                        Some(dir) => dir.canonicalize().map_err(|e| {
                            format!("cannot resolve directory for \"{file_path}\": {e}")
                        })?,
                        None => std::env::current_dir()
                            .map_err(|e| format!("cannot determine working directory: {e}"))?,
                    }
                }
            }
        };

        // Type errors are advisory unless --strict is set.
        // Build type environment with prelude + includes (if file-based).
        let type_env = match stage {
            PipelineStage::File(file_path) if file_path != "-" => {
                // File-based: use build_type_env with base_dir for include resolution
                let (env, _include_bindings) =
                    tinct::build_type_env(&ast.node, Some(&file_base_dir_path));
                env
            }
            _ => {
                // Stdin or inline expr: prelude-only (no include resolution)
                tinct::build_prelude_env()
            }
        };
        let (
            type_errors,
            _type_map,
            _doc_map,
            _scheme_map,
            type_diagnostics,
            infer_state,
            _final_env,
        ) = tinct::typecheck::typecheck_file_with_types_and_env_and_source_returning_state(
            &ast.node, type_env, false, // disable scheme_map (not needed for eval)
            false, // not in prelude load
        );
        if !type_errors.is_empty() {
            let file_name = match stage {
                PipelineStage::File(fp) => fp.as_str(),
                PipelineStage::Expr(_) => "<expr>",
            };
            for err in &type_errors {
                eprintln!("{}", tinct::format_type_error(err, &source, file_name));
            }
            if strict {
                // In strict mode, type errors are fatal — exit.
                return Err("type checking failed — cannot evaluate".to_string());
            } else {
                // Non-strict mode: type errors are advisory, print warning and continue.
                eprintln!(
                    "type checking failed with {} error(s) (use --strict to make fatal)",
                    type_errors.len()
                );
            }
        }
        // Emit type quality diagnostics (T010/T011 Unknown, T012 overbroad, T013 ambiguous, …).
        // These are advisory and never block evaluation, even in --strict mode.
        if !type_diagnostics.is_empty() {
            let diag_file_name = match stage {
                PipelineStage::File(fp) => fp.as_str(),
                PipelineStage::Expr(_) => "<expr>",
            };
            for d in &type_diagnostics {
                eprintln!("{}", format_type_diagnostic(d, &source, diag_file_name));
            }
        }

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
            let ctx = EvalContext::new_with_options(
                base_dir,
                Rc::clone(&env),
                no_fs,
                require_integrity,
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

        // Wire boundary guards and do-infer resolutions from type inference to the eval context
        eval_ctx.set_boundary_guards(infer_state.boundary_guards);
        eval_ctx.set_do_infer_resolutions(infer_state.do_infer_resolutions);

        // Evaluate file with pipeline input
        let file_result =
            eval_file_with_input(&ast.node, Rc::clone(&env), &eval_ctx, pipeline_input).map_err(
                |e| {
                    let mut error_str = format!("{e}");
                    if let Some(snippet) = tinct::render_span_snippet(&source, e.definition_span) {
                        error_str.push('\n');
                        error_str.push_str(&snippet);
                    }
                    error_str
                },
            )?;

        // Record blame provenance for the pipeline boundary.
        // The producing stage label is the file path or expression index.
        let stage_label = match stage {
            PipelineStage::File(p) => p.clone(),
            PipelineStage::Expr(_) => format!("(inline expression)"),
        };

        // Pass the result as lazy thunk to next file (matching --- boundary semantics).
        // Because all files share the same ThunkArena, the ThunkIds in file_result are
        // valid in the next file's eval context.
        pipeline_input = Some(file_result.clone());

        // Record blame for the % thunk at this pipeline boundary.
        // This is used by contract violation errors to identify the producing stage.
        if let Ok(val) = tinct::materialize(&file_result, None, &eval_ctx) {
            if let tinct::Value::Dict(ref map) = val {
                for (_, thunk_id) in map {
                    eval_ctx.record_blame(*thunk_id, stage_label.clone());
                }
            }
        }
        // Also record blame for the result thunk itself
        // (use the thunk_arena alloc id if available)
        let _ = stage_label; // label used above

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
    let val = materialize(&thunk, None, &eval_ctx).map_err(|e| {
        let mut error_str = format!("{e}");
        if let Some(snippet) = tinct::render_span_snippet(&last_source, e.definition_span) {
            error_str.push('\n');
            error_str.push_str(&snippet);
        }
        error_str
    })?;

    // Deep-force all thunks when:
    // - --eval flag was given (explicit deep materialization)
    // - -o flag was given (output formatter stage may contain lazy emit calls inside a Dict
    //   module; deep materialization forces all entries, triggering the emit side effects)
    let _val = if force_eval || output.is_some() {
        deep_materialize(&val, &eval_ctx, None).map_err(|e| {
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

    // Serialize and output
    // When no -o flag is given, no JSON output is produced (emit-only mode).
    // The -o flag appends an output formatter to the pipeline, so we never reach this point
    // with output.is_some(). This block only runs when there was NO -o flag.
    if output.is_none() {
        // No -o flag was given.
        // Emit-only mode: print nothing (the user should use emit or -o).
        // This is the default behavior when no output format is specified.
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

fn run_fmt(
    file_path: &str,
    check: bool,
    in_place: bool,
    output_name: &str,
    strict: bool,
) -> Result<(), String> {
    let source = read_source(file_path)?;

    // If --strict is set, typecheck the file first and fail if type errors exist.
    // Parse once and run the type checking pipeline on the parsed AST.
    // This avoids the double-parse that would happen if we called typecheck_source().
    if strict {
        let ast = parse(&source)
            .map(|o| o.file)
            .map_err(|e| tinct::format_parse_error(&e, &source, file_path))?;

        // PIPELINE INVARIANT: expand_macros -> desugar -> resolve -> typecheck.
        // See also: src/lib.rs (typecheck_source pipeline)
        let expand_result = tinct::expand::expand_macros(ast, false).map_err(|e| format!("{e}"))?;
        let mut ast = expand_result.file;

        tinct::desugar::desugar_file(&mut ast.node);
        tinct::resolve::resolve_file(&ast.node);

        let env = tinct::build_prelude_env();
        let (type_errors, _type_map, _doc_map, _scheme_map, fmt_diagnostics) =
            tinct::typecheck::typecheck_file_with_types_and_env(&ast.node, env);

        if !type_errors.is_empty() {
            let error_msgs: Vec<String> = type_errors
                .iter()
                .map(|e| tinct::format_type_error(e, &source, file_path))
                .collect();
            return Err(error_msgs.join("\n"));
        }

        // Emit type quality diagnostics (T010/T011 Unknown, T012 overbroad, T013 ambiguous, …).
        // These are advisory warnings and do not block formatting even in --strict mode.
        for d in &fmt_diagnostics {
            eprintln!("{}", format_type_diagnostic(d, &source, file_path));
        }
    }

    // Resolve the formatter script from %libdir/cli/fmt/<name>.llt.
    let script_path = {
        let libdir = find_libdir_path().ok_or_else(|| {
            format!("stdlib directory not found — cannot locate formatter script '{output_name}'")
        })?;
        let path = libdir
            .join("cli")
            .join("fmt")
            .join(format!("{output_name}.llt"));
        if !path.exists() {
            return Err(format!(
                "fmt: formatter script not found: {} (resolved from -o '{output_name}')",
                path.display()
            ));
        }
        path
    };

    // Format the source using the tinct-hosted formatter.
    // The formatter re-parses internally; we cannot reuse the typecheck AST because
    // the formatter needs to preserve comments and layout details.
    let formatted = tinct::format_source_tinct(&source, &script_path)?;

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

/// Type-check a file without evaluating.
/// Exit 0 on clean, exit 1 on any warnings or errors.
fn run_lint(
    file_path: &str,
    _no_fs: bool,
    _cap_fs: &[String],
    _cap_net: &[String],
) -> Result<(), String> {
    let source = read_source(file_path)?;

    // Parse the file
    let ast = parse(&source)
        .map(|o| o.file)
        .map_err(|e| tinct::format_parse_error(&e, &source, file_path))?;

    // PIPELINE INVARIANT: expand_macros -> desugar -> resolve -> typecheck.
    let expand_result = tinct::expand::expand_macros(ast, false).map_err(|e| format!("{e}"))?;
    let mut ast = expand_result.file;

    tinct::desugar::desugar_file(&mut ast.node);
    tinct::resolve::resolve_file(&ast.node);

    // Type check with prelude environment
    let env = tinct::build_prelude_env();
    let (type_errors, _type_map, _doc_map, _scheme_map, diagnostics) =
        tinct::typecheck::typecheck_file_with_types_and_env(&ast.node, env);

    // Collect all errors and warnings
    let mut all_messages = Vec::new();

    for e in &type_errors {
        all_messages.push(tinct::format_type_error(e, &source, file_path));
    }

    for d in &diagnostics {
        all_messages.push(format_type_diagnostic(d, &source, file_path));
    }

    if !all_messages.is_empty() {
        // Print all errors/warnings to stderr
        eprintln!("{}", all_messages.join("\n"));
        return Err(format!("lint failed with {} issue(s)", all_messages.len()));
    }

    // Clean file — exit 0 (no output)
    Ok(())
}

/// Format a TypeDiagnostic with source context for display.
/// Similar to format_type_error but handles the diagnostic level (info/warn/err).
fn format_type_diagnostic(diag: &tinct::TypeDiagnostic, source: &str, file_name: &str) -> String {
    use tinct::DiagnosticLevel;

    let level_str = match diag.level {
        DiagnosticLevel::Info => "info",
        DiagnosticLevel::Warn => "warning",
        DiagnosticLevel::Err => "error",
    };

    let code = diag.code;
    let line = diag.span.start.line;
    let col = diag.span.start.column;

    // Header: level[Txxx]: message
    let mut out = format!("{level_str}[{code}]: {}\n", diag.message);

    // Location: --> file:line:col
    out.push_str(&format!(" --> {file_name}:{line}:{col}\n"));

    // Snippet: source context with caret
    if let Some(snippet) = tinct::render_span_snippet(source, diag.span) {
        out.push_str("  |\n");
        out.push_str(&snippet);
    }

    out
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
#[allow(clippy::too_many_arguments)]
fn run_literate(
    file_path: &str,
    mode: &LiterateMode,
    no_substitute: bool,
    strict: bool,
    in_place: bool,
    verify: bool,
    fail_on_errors: bool,
    cap_fs: &[String],
    cap_net: &[String],
) -> Result<(), String> {
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
            run_literate_eval(&tangled, file_path, strict, cap_fs, cap_net)
        }

        LiterateMode::Weave => run_literate_weave(
            &markdown,
            &blocks,
            file_path,
            no_substitute,
            strict,
            in_place,
            verify,
            fail_on_errors,
            cap_fs,
            cap_net,
        ),
    }
}

/// Evaluate a tangled tinct source string and print the result as JSON.
///
/// Reuses the same pipeline as `run_eval` (parse → desugar → resolve →
/// typecheck → eval → materialize → JSON).
/// The base directory is derived from the Markdown file's parent directory.
///
/// Literate mode always runs with --no-pwd and --no-env (hard-coded).
/// Capabilities are injected via cap_fs and cap_net. %libdir is always available.
/// %clock is set to a fixed ClockCap from the markdown file's mtime.
#[allow(clippy::too_many_arguments)]
fn run_literate_eval(
    tangled: &str,
    markdown_path: &str,
    strict: bool,
    cap_fs: &[String],
    cap_net: &[String],
) -> Result<(), String> {
    // Parse the tangled source.
    let ast = parse(tangled).map(|o| o.file).map_err(|e| {
        if strict {
            tinct::format_parse_error(&e, tangled, markdown_path)
        } else {
            format!("parse error in tangled tinct source: {e}")
        }
    })?;

    // Expand macros (pre-desugar AST transformation).
    let expand_result = tinct::expand::expand_macros(ast, false).map_err(|e| format!("{e}"))?;
    let mut ast = expand_result.file;

    tinct::desugar::desugar_file(&mut ast.node);
    tinct::resolve::resolve_file(&ast.node);
    let (type_errors, _diagnostics) = tinct::typecheck::typecheck_file(&ast.node);

    // In strict mode, type errors are fatal
    if strict && !type_errors.is_empty() {
        let mut msg = String::from("type errors:\n");
        for err in &type_errors {
            msg.push_str(&format!("  {err}\n"));
        }
        return Err(msg);
    }

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

    // E1: Inject fixed ClockCap from file mtime for deterministic output
    {
        use tinct::{ClockCapInner, Value};

        // Get the markdown file's mtime
        let mtime = if markdown_path == "-" {
            // For stdin, use Unix epoch as a stable default
            jiff::Timestamp::from_second(0)
                .map_err(|e| format!("failed to create epoch timestamp: {e}"))?
        } else {
            let metadata = std::fs::metadata(markdown_path)
                .map_err(|e| format!("cannot read file metadata: {e}"))?;
            let system_time = metadata
                .modified()
                .map_err(|e| format!("cannot read file mtime: {e}"))?;

            // Convert SystemTime to jiff::Timestamp
            let duration_since_epoch = system_time
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| format!("file mtime is before Unix epoch: {e}"))?;
            let nanos = i128::try_from(duration_since_epoch.as_nanos())
                .map_err(|_| "mtime nanoseconds out of i128 range".to_string())?;
            jiff::Timestamp::from_nanosecond(nanos)
                .map_err(|e| format!("failed to convert mtime to timestamp: {e}"))?
        };

        let nanos = i64::try_from(mtime.as_nanosecond())
            .map_err(|_| format!("mtime is out of i64 range"))?;
        let cap_value = Value::ClockCap(Rc::new(ClockCapInner::Fixed(nanos)));
        let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
        env.borrow_mut()
            .insert("%clock".to_string(), Rc::new(cap_thunk));
    }

    // E3: Inject --cap-fs NAME=PATH[:MODE] entries (same as run_eval)
    {
        use tinct::{DirPerms, Value};
        for cap_fs_entry in cap_fs {
            let (name, path_and_mode) = cap_fs_entry.split_once('=').ok_or_else(|| {
                format!(
                    "--cap-fs: expected NAME=PATH[:MODE] format, got {:?}",
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

            // Split PATH:MODE on the last colon
            let (path_str, mode_str) = match path_and_mode.rsplit_once(':') {
                Some((path, mode)) => (path, Some(mode)),
                None => (path_and_mode, None),
            };

            let cap_path = std::path::Path::new(path_str.trim());
            let cap_dir =
                cap_std::fs::Dir::open_ambient_dir(cap_path, cap_std::ambient_authority())
                    .map_err(|e| {
                        format!(
                            "--cap-fs: cannot open directory {:?}: {e}",
                            cap_path.display()
                        )
                    })?;

            // Parse mode into DirPerms (letter mode: r/w/a/s/l or extended [Cap1 ...])
            let perms = if let Some(mode) = mode_str {
                let mode = mode.trim();
                if mode.starts_with('[') {
                    // Extended syntax: [Readable Writable ...]
                    if !mode.ends_with(']') {
                        return Err(format!(
                            "--cap-fs: extended mode must end with ']', got {:?}",
                            mode
                        ));
                    }
                    let caps_str = &mode[1..mode.len() - 1];
                    let mut perms = DirPerms {
                        readable: false,
                        statable: false,
                        listable: false,
                        writable: false,
                        appendable: false,
                        deletable: false,
                        renameable: false,
                    };
                    for cap_name in caps_str.split_whitespace() {
                        match cap_name {
                            "Readable" => perms.readable = true,
                            "Statable" => perms.statable = true,
                            "Listable" => perms.listable = true,
                            "Writable" => perms.writable = true,
                            "Appendable" => perms.appendable = true,
                            "Deletable" => perms.deletable = true,
                            "Renameable" => perms.renameable = true,
                            _ => {
                                return Err(format!(
                                    "--cap-fs: unknown capability {:?} in extended mode",
                                    cap_name
                                ))
                            }
                        }
                    }
                    perms
                } else {
                    // Letter mode: r/w/a/s/l
                    let mut perms = DirPerms {
                        readable: false,
                        statable: false,
                        listable: false,
                        writable: false,
                        appendable: false,
                        deletable: false,
                        renameable: false,
                    };
                    for c in mode.chars() {
                        if let Some(letter_perms) = DirPerms::from_letter(c) {
                            perms = perms.union(&letter_perms);
                        } else {
                            return Err(format!(
                                "--cap-fs: unknown mode letter {:?} (expected r/w/a/s/l)",
                                c
                            ));
                        }
                    }
                    perms
                }
            } else {
                // No mode specified → full access
                DirPerms::full()
            };

            let cap_value = Value::DirCap {
                dir: Rc::new(cap_dir),
                perms,
            };
            let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
            // Inject as `%NAME` (auto-prefix %).
            let scoped_name = if name.starts_with('%') {
                name.to_string()
            } else {
                format!("%{name}")
            };
            env.borrow_mut().insert(scoped_name, Rc::new(cap_thunk));
        }
    }

    // E3: Inject --cap-net NAME=ENTRY entries (same as run_eval)
    {
        use std::collections::HashMap;
        use tinct::NetCapEntry;
        use tinct::Value;

        let mut net_caps: HashMap<String, Vec<NetCapEntry>> = HashMap::new();

        for cap_net_entry in cap_net {
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

            let entry = parse_cli_net_cap_entry(entry_str)?;
            let scoped_name = if name.starts_with('%') {
                name.to_string()
            } else {
                format!("%{name}")
            };
            net_caps.entry(scoped_name).or_default().push(entry);
        }

        // Now bind each accumulated NetCap.
        for (name, entries) in net_caps {
            let cap_value = Value::NetCap(Rc::new(entries));
            let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
            env.borrow_mut().insert(name, Rc::new(cap_thunk));
        }
    }

    // Literate mode always runs with --no-env (hard-coded, per doc comment).
    // env_allowed: Some(empty) = all env vars denied.
    let eval_ctx = EvalContext::new_with_options(
        base_dir,
        Rc::clone(&env),
        false,
        false,
        Some(std::collections::HashSet::new()),
    );

    let thunk = eval_file_with_input(&ast.node, Rc::clone(&env), &eval_ctx, None).map_err(|e| {
        let mut msg = format!("{e}");
        if let Some(snippet) = tinct::render_span_snippet(tangled, e.definition_span) {
            msg.push('\n');
            msg.push_str(&snippet);
        }
        msg
    })?;

    let val = materialize(&thunk, None, &eval_ctx).map_err(|e| {
        let mut msg = format!("{e}");
        if let Some(snippet) = tinct::render_span_snippet(tangled, e.definition_span) {
            msg.push('\n');
            msg.push_str(&snippet);
        }
        msg
    })?;

    // Always serialize to JSON (emit is purely additive)
    // Use the same format_with_json_llt → fallback pattern as run_eval for consistent
    // null semantics ([] → JSON null, not {}).
    let json_llt_path = find_libdir_path().map(|p| p.join("cli").join("out").join("json.llt"));

    let output = if let Some(ref json_llt_path) = json_llt_path {
        match format_with_json_llt(Rc::clone(&thunk), &eval_ctx, Rc::clone(&env), json_llt_path) {
            Ok(Some(compact_json)) => {
                let parsed: serde_json::Value =
                    serde_json::from_str(&compact_json).map_err(|e| {
                        format!("json.llt produced invalid JSON: {e}\noutput: {compact_json}")
                    })?;
                serde_json::to_string_pretty(&parsed)
                    .map_err(|e| format!("JSON pretty-print error: {e}"))?
            }
            Ok(None) => {
                let json = value_to_json(&val, &eval_ctx).map_err(|e| format!("{e}"))?;
                serde_json::to_string_pretty(&json)
                    .map_err(|e| format!("JSON serialization error: {e}"))?
            }
            Err(e) => return Err(e),
        }
    } else {
        let json = value_to_json(&val, &eval_ctx).map_err(|e| format!("{e}"))?;
        serde_json::to_string_pretty(&json).map_err(|e| format!("JSON serialization error: {e}"))?
    };

    println!("{output}");

    Ok(())
}

/// Weave mode: evaluate blocks and update/verify === sections in code blocks.
///
/// Evaluates each block in pipeline order, threading `%` between blocks.
///
/// **Modes:**
/// - Default (no flags): embed errors in `=== error` section, continue to next block, exit 0
/// - `--fail-on-errors`: any evaluation error exits 1 immediately
/// - `--verify`: compare actual output against expected === sections; exit 1 on mismatch
/// - `--in-place`: write output to .tmp then rename to source file (instead of stdout)
///
/// **Literate-specific behavior:**
/// - Always runs with --no-pwd and --no-env (hard-coded)
/// - %clock is set to a fixed ClockCap from markdown file mtime
/// - %libdir is always available
/// - Capabilities injected via cap_fs and cap_net
#[allow(clippy::too_many_arguments)]
fn run_literate_weave(
    markdown: &str,
    blocks: &[String],
    markdown_path: &str,
    no_substitute: bool,
    strict: bool,
    in_place: bool,
    verify: bool,
    fail_on_errors: bool,
    cap_fs: &[String],
    cap_net: &[String],
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

    // E1: Inject fixed ClockCap from file mtime for deterministic weave output
    {
        use tinct::{ClockCapInner, Value};

        // Get the markdown file's mtime
        let mtime = if markdown_path == "-" {
            // For stdin, use Unix epoch as a stable default
            jiff::Timestamp::from_second(0)
                .map_err(|e| format!("failed to create epoch timestamp: {e}"))?
        } else {
            let metadata = std::fs::metadata(markdown_path)
                .map_err(|e| format!("cannot read file metadata: {e}"))?;
            let system_time = metadata
                .modified()
                .map_err(|e| format!("cannot read file mtime: {e}"))?;

            // Convert SystemTime to jiff::Timestamp
            let duration_since_epoch = system_time
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| format!("file mtime is before Unix epoch: {e}"))?;
            let nanos = i128::try_from(duration_since_epoch.as_nanos())
                .map_err(|_| "mtime nanoseconds out of i128 range".to_string())?;
            jiff::Timestamp::from_nanosecond(nanos)
                .map_err(|e| format!("failed to convert mtime to timestamp: {e}"))?
        };

        let nanos = i64::try_from(mtime.as_nanosecond())
            .map_err(|_| format!("mtime is out of i64 range"))?;
        let cap_value = Value::ClockCap(Rc::new(ClockCapInner::Fixed(nanos)));
        let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
        env.borrow_mut()
            .insert("%clock".to_string(), Rc::new(cap_thunk));
    }

    // E3: Inject --cap-fs NAME=PATH[:MODE] entries (same as run_eval)
    {
        use tinct::{DirPerms, Value};
        for cap_fs_entry in cap_fs {
            let (name, path_and_mode) = cap_fs_entry.split_once('=').ok_or_else(|| {
                format!(
                    "--cap-fs: expected NAME=PATH[:MODE] format, got {:?}",
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

            // Split PATH:MODE on the last colon
            let (path_str, mode_str) = match path_and_mode.rsplit_once(':') {
                Some((path, mode)) => (path, Some(mode)),
                None => (path_and_mode, None),
            };

            let cap_path = std::path::Path::new(path_str.trim());
            let cap_dir =
                cap_std::fs::Dir::open_ambient_dir(cap_path, cap_std::ambient_authority())
                    .map_err(|e| {
                        format!(
                            "--cap-fs: cannot open directory {:?}: {e}",
                            cap_path.display()
                        )
                    })?;

            // Parse mode into DirPerms (letter mode: r/w/a/s/l or extended [Cap1 ...])
            let perms = if let Some(mode) = mode_str {
                let mode = mode.trim();
                if mode.starts_with('[') {
                    // Extended syntax: [Readable Writable ...]
                    if !mode.ends_with(']') {
                        return Err(format!(
                            "--cap-fs: extended mode must end with ']', got {:?}",
                            mode
                        ));
                    }
                    let caps_str = &mode[1..mode.len() - 1];
                    let mut perms = DirPerms {
                        readable: false,
                        statable: false,
                        listable: false,
                        writable: false,
                        appendable: false,
                        deletable: false,
                        renameable: false,
                    };
                    for cap_name in caps_str.split_whitespace() {
                        match cap_name {
                            "Readable" => perms.readable = true,
                            "Statable" => perms.statable = true,
                            "Listable" => perms.listable = true,
                            "Writable" => perms.writable = true,
                            "Appendable" => perms.appendable = true,
                            "Deletable" => perms.deletable = true,
                            "Renameable" => perms.renameable = true,
                            _ => {
                                return Err(format!(
                                    "--cap-fs: unknown capability {:?} in extended mode",
                                    cap_name
                                ))
                            }
                        }
                    }
                    perms
                } else {
                    // Letter mode: r/w/a/s/l
                    let mut perms = DirPerms {
                        readable: false,
                        statable: false,
                        listable: false,
                        writable: false,
                        appendable: false,
                        deletable: false,
                        renameable: false,
                    };
                    for c in mode.chars() {
                        if let Some(letter_perms) = DirPerms::from_letter(c) {
                            perms = perms.union(&letter_perms);
                        } else {
                            return Err(format!(
                                "--cap-fs: unknown mode letter {:?} (expected r/w/a/s/l)",
                                c
                            ));
                        }
                    }
                    perms
                }
            } else {
                // No mode specified → full access
                DirPerms::full()
            };

            let cap_value = Value::DirCap {
                dir: Rc::new(cap_dir),
                perms,
            };
            let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
            // Inject as `%NAME` (auto-prefix %).
            let scoped_name = if name.starts_with('%') {
                name.to_string()
            } else {
                format!("%{name}")
            };
            env.borrow_mut().insert(scoped_name, Rc::new(cap_thunk));
        }
    }

    // E3: Inject --cap-net NAME=ENTRY entries (same as run_eval)
    {
        use std::collections::HashMap;
        use tinct::NetCapEntry;
        use tinct::Value;

        let mut net_caps: HashMap<String, Vec<NetCapEntry>> = HashMap::new();

        for cap_net_entry in cap_net {
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

            let entry = parse_cli_net_cap_entry(entry_str)?;
            let scoped_name = if name.starts_with('%') {
                name.to_string()
            } else {
                format!("%{name}")
            };
            net_caps.entry(scoped_name).or_default().push(entry);
        }

        // Now bind each accumulated NetCap.
        for (name, entries) in net_caps {
            let cap_value = Value::NetCap(Rc::new(entries));
            let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
            env.borrow_mut().insert(name, Rc::new(cap_thunk));
        }
    }

    // Create one base EvalContext that owns the shared ThunkArena.
    // All blocks derive from this context via with_base_dir_and_path so that
    // ThunkIds allocated by block N remain valid when block N+1 references them
    // via the % pipeline variable. This matches the arena-sharing pattern used by
    // the multi-file pipeline in run_eval.
    let base_dir_initial =
        cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
            .map_err(|e| format!("cannot open base directory: {e}"))?;
    // Literate mode always runs with --no-env (hard-coded, per doc comment).
    // env_allowed: Some(empty) = all env vars denied.
    let base_eval_ctx = EvalContext::new_with_options(
        base_dir_initial,
        Rc::clone(&env),
        false,
        false,
        Some(std::collections::HashSet::new()),
    );

    // Evaluate each block in turn, passing the previous result as pipeline input.
    // Collect (block_index -> actual output sections) for weaving/verification.
    let mut pipeline_input: Option<Rc<Thunk>> = None;

    // Split blocks into code + expectations
    let blocks_with_exp: Vec<_> = blocks
        .iter()
        .map(|b| literate::split_block_sections(b))
        .collect();

    let mut block_outputs: Vec<BlockOutput> = Vec::with_capacity(blocks_with_exp.len());

    for (i, block_with_exp) in blocks_with_exp.iter().enumerate() {
        let code = &block_with_exp.code;

        let parse_result = parse(code).map(|o| o.file);
        let ast = match parse_result {
            Ok(a) => a,
            Err(e) => {
                let error_msg = if strict {
                    tinct::format_parse_error(&e, code, &format!("block {}", i + 1))
                } else {
                    format!("{e}")
                };

                if fail_on_errors {
                    return Err(format!("parse error in code block {}: {error_msg}", i + 1));
                }

                // Embed error and continue
                block_outputs.push(BlockOutput {
                    out: None,
                    warn: None,
                    error: Some(error_msg),
                    info: None,
                });
                continue;
            }
        };

        // Expand macros (pre-desugar AST transformation).
        let expand_result = match tinct::expand::expand_macros(ast, false) {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("{e}");
                if fail_on_errors {
                    return Err(format!(
                        "macro expansion error in code block {}: {msg}",
                        i + 1
                    ));
                }
                block_outputs.push(BlockOutput {
                    out: None,
                    warn: None,
                    error: Some(msg),
                    info: None,
                });
                continue;
            }
        };
        let mut ast = expand_result.file;

        tinct::desugar::desugar_file(&mut ast.node);
        tinct::resolve::resolve_file(&ast.node);
        let (type_errors, _diagnostics) = tinct::typecheck::typecheck_file(&ast.node);

        // Capture type warnings (always non-fatal in literate mode unless --strict)
        let type_warnings = if !strict && !type_errors.is_empty() {
            let mut msg = String::new();
            for err in &type_errors {
                msg.push_str(&format!("{err}\n"));
            }
            Some(msg.trim_end().to_string())
        } else {
            None
        };

        // In strict mode, type errors are fatal
        if strict && !type_errors.is_empty() {
            let mut msg = String::from("type errors:\n");
            for err in &type_errors {
                msg.push_str(&format!("  {err}\n"));
            }
            if fail_on_errors {
                return Err(format!("type errors in code block {}: {msg}", i + 1));
            }
            block_outputs.push(BlockOutput {
                out: None,
                warn: None,
                error: Some(msg),
                info: None,
            });
            continue;
        }

        // Derive per-block context from the base context (shares the ThunkArena).
        let base_dir =
            cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
                .map_err(|e| format!("cannot open base directory: {e}"))?;

        let eval_ctx = base_eval_ctx.with_base_dir_and_path(base_dir, Some(base_dir_path.clone()));

        let thunk_result = eval_file_with_input(
            &ast.node,
            Rc::clone(&env),
            &eval_ctx,
            pipeline_input.clone(),
        );
        let thunk = match thunk_result {
            Ok(t) => t,
            Err(e) => {
                let error_msg = format!("{e}");
                if fail_on_errors {
                    return Err(format!("eval error in code block {}: {error_msg}", i + 1));
                }
                block_outputs.push(BlockOutput {
                    out: None,
                    warn: type_warnings,
                    error: Some(error_msg),
                    info: None,
                });
                continue;
            }
        };

        let val_result = materialize(&thunk, None, &eval_ctx);
        let val = match val_result {
            Ok(v) => v,
            Err(e) => {
                let error_msg = format!("{e}");
                if fail_on_errors {
                    return Err(format!(
                        "materialize error in code block {}: {error_msg}",
                        i + 1
                    ));
                }
                block_outputs.push(BlockOutput {
                    out: None,
                    warn: type_warnings,
                    error: Some(error_msg),
                    info: None,
                });
                continue;
            }
        };

        // Always serialize the result to JSON (emit is additive)
        let json = value_to_json(&val, &eval_ctx)
            .map_err(|e| format!("error serializing code block {} result: {e}", i + 1))?;
        let output_str = serde_json::to_string(&json)
            .map_err(|e| format!("JSON serialization error in block {}: {e}", i + 1))?;

        block_outputs.push(BlockOutput {
            out: Some(output_str),
            warn: type_warnings,
            error: None,
            info: None,
        });
        // Thread the result as pipeline input to the next block.
        pipeline_input = Some(Rc::clone(&thunk));
    }

    // C3: Verify mode — compare actual output against expected === sections
    if verify {
        let mut mismatches = Vec::new();
        for (i, (block_with_exp, block_output)) in
            blocks_with_exp.iter().zip(block_outputs.iter()).enumerate()
        {
            let expected = &block_with_exp.expectations;

            // Check output section
            if let Some(ref expected_out) = expected.out {
                match &block_output.out {
                    Some(actual_out) => {
                        if actual_out.trim() != expected_out.trim() {
                            mismatches.push(format!(
                                "Block {}: output mismatch\nExpected:\n{}\nActual:\n{}",
                                i + 1,
                                expected_out,
                                actual_out
                            ));
                        }
                    }
                    None => {
                        mismatches.push(format!(
                            "Block {}: expected output but got error\nExpected:\n{}\nActual error:\n{}",
                            i + 1, expected_out, block_output.error.as_ref().unwrap_or(&"(no error message)".to_string())
                        ));
                    }
                }
            }

            // Check warn section
            if let Some(ref expected_warn) = expected.warn {
                match &block_output.warn {
                    Some(actual_warn) => {
                        if !actual_warn.contains(expected_warn.trim()) {
                            mismatches.push(format!(
                                "Block {}: warning mismatch\nExpected substring:\n{}\nActual:\n{}",
                                i + 1,
                                expected_warn,
                                actual_warn
                            ));
                        }
                    }
                    None => {
                        mismatches.push(format!(
                            "Block {}: expected warning but got none\nExpected warning:\n{}",
                            i + 1,
                            expected_warn
                        ));
                    }
                }
            }

            // Check error section
            if let Some(ref expected_error) = expected.error {
                match &block_output.error {
                    Some(actual_error) => {
                        if !actual_error.contains(expected_error.trim()) {
                            mismatches.push(format!(
                                "Block {}: error mismatch\nExpected substring:\n{}\nActual:\n{}",
                                i + 1,
                                expected_error,
                                actual_error
                            ));
                        }
                    }
                    None => {
                        mismatches.push(format!(
                            "Block {}: expected error but got success\nExpected error:\n{}",
                            i + 1,
                            expected_error
                        ));
                    }
                }
            }

            // Check info section
            if let Some(ref expected_info) = expected.info {
                match &block_output.info {
                    Some(actual_info) => {
                        if !actual_info.contains(expected_info.trim()) {
                            mismatches.push(format!(
                                "Block {}: info mismatch\nExpected substring:\n{}\nActual:\n{}",
                                i + 1,
                                expected_info,
                                actual_info
                            ));
                        }
                    }
                    None => {
                        mismatches.push(format!(
                            "Block {}: expected info but got none\nExpected info:\n{}",
                            i + 1,
                            expected_info
                        ));
                    }
                }
            }
        }

        if !mismatches.is_empty() {
            eprintln!(
                "Verification failed with {} mismatche(s):\n",
                mismatches.len()
            );
            for mismatch in &mismatches {
                eprintln!("{}\n", mismatch);
            }
            return Err("verification failed".to_string());
        }

        // Verification passed
        return Ok(());
    }

    // C2: Weave mode — reconstruct blocks with === sections inline
    let mut block_idx = 0;
    let mut in_tinct_block = false;
    let mut in_code_portion = false;
    let mut output = String::with_capacity(markdown.len() + block_outputs.len() * 80);
    let lines: Vec<&str> = markdown.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if !in_tinct_block {
            output.push_str(line);
            output.push('\n');
            if trimmed == "```tinct" || trimmed == "```llt" {
                in_tinct_block = true;
                in_code_portion = true;
            }
        } else if trimmed == "```" {
            // Closing fence for tinct block
            in_tinct_block = false;
            in_code_portion = false;

            // C2: Insert === sections before closing fence
            if block_idx < block_outputs.len() {
                let block_output = &block_outputs[block_idx];

                // Add === warn section if there are warnings
                if let Some(ref warn) = block_output.warn {
                    output.push_str("=== warn\n");
                    output.push_str(warn);
                    output.push('\n');
                }

                // Add === out section if there's output
                if let Some(ref out) = block_output.out {
                    output.push_str("=== out\n");
                    output.push_str(out);
                    output.push('\n');
                }

                // Add === error section if there's an error
                if let Some(ref error) = block_output.error {
                    output.push_str("=== error\n");
                    output.push_str(error);
                    output.push('\n');
                }

                // Add === info section if there's info/log output
                if let Some(ref info) = block_output.info {
                    output.push_str("=== info\n");
                    output.push_str(info);
                    output.push('\n');
                }

                block_idx += 1;
            }

            output.push_str(line);
            output.push('\n');
        } else if in_code_portion && trimmed.starts_with("===") {
            // Skip existing === sections and everything after them in this block
            in_code_portion = false;
            // Don't write this line or any subsequent lines until closing fence
        } else if in_code_portion {
            // Still in code portion — keep the line
            output.push_str(line);
            output.push('\n');
        }
        // else: skip lines in old === sections

        i += 1;
    }

    // Note: no_substitute is ignored. All docs use === out sections now.
    let _ = no_substitute; // silence unused warning

    // Write output
    if in_place {
        write_file_atomic(markdown_path, &output)?;
    } else {
        print!("{output}");
    }

    Ok(())
}

/// Substitute inline `<!-- tinct-result: ... -->` markers in Markdown output.
///
/// Markers with an expression (e.g., `<!-- tinct-result: expr -->`) evaluate that expression
/// with the most recent block result available as `%`. Markers without an expression
/// (just `<!-- tinct-result -->`) are replaced with the most recent block result.
///
/// This function scans the output for markers and replaces them with evaluated results.
///
/// Note: With === sections, this is only used for backward compatibility with old markdown files.
/// The new format does not emit HTML comments.
/// Schema keys recognized by the `describe` subcommand heuristic.
/// A dict is considered a "schema dict" if any of its values is a dict containing
/// at least one of these keys. This mirrors the constraint keys supported by `$validate`.
const SCHEMA_KEYS: &[&str] = &[
    "type",
    "min",
    "max",
    "min-length",
    "max-length",
    "pattern",
    "required",
    "items",
    "fields",
    "enum",
];

/// Describe the input contract of an LLT file.
///
/// Parses the file, extracts `%@Type` / `expects:` annotations from each document,
/// and detects schema dicts by heuristic. Outputs a human-readable summary (default)
/// or machine-readable JSON (`--json`).
/// Write content to a file atomically using a .tmp file then rename.
fn write_file_atomic(path: &str, content: &str) -> Result<(), String> {
    use std::io::Write;

    let tmp_path = format!("{}.tmp", path);
    let mut file = std::fs::File::create(&tmp_path)
        .map_err(|e| format!("cannot create temporary file {}: {e}", tmp_path))?;

    file.write_all(content.as_bytes())
        .map_err(|e| format!("cannot write to temporary file {}: {e}", tmp_path))?;

    file.sync_all()
        .map_err(|e| format!("cannot sync temporary file {}: {e}", tmp_path))?;

    std::fs::rename(&tmp_path, path)
        .map_err(|e| format!("cannot rename {} to {}: {e}", tmp_path, path))?;

    Ok(())
}

fn run_describe(file_path: &str, json_mode: bool) -> Result<(), String> {
    let source = read_source(file_path)?;
    let ast = parse(&source).map(|o| o.file).map_err(|e| format!("{e}"))?;

    // PIPELINE INVARIANT: expand_macros -> desugar -> resolve -> typecheck.
    let expand_result = tinct::expand::expand_macros(ast, false).map_err(|e| format!("{e}"))?;
    let mut ast = expand_result.file;

    tinct::desugar::desugar_file(&mut ast.node);
    tinct::resolve::resolve_file(&ast.node);

    // Type check to get DocMap (for doc strings)
    let env = tinct::build_prelude_env();
    let (_type_errors, _type_map, doc_map, _scheme_map, _diagnostics) =
        tinct::typecheck::typecheck_file_with_types_and_env(&ast.node, env);

    // Collect contract information from each document section.
    let mut contracts: Vec<serde_json::Value> = Vec::new();
    let mut has_any_contract = false;

    for (doc_idx, doc) in ast.node.documents.iter().enumerate() {
        let mut doc_contract = serde_json::Map::new();
        doc_contract.insert("section".into(), serde_json::json!(doc_idx));

        // Extract expects: / %@Type annotation
        if let Some(ref ann) = doc.node.expects {
            has_any_contract = true;
            match &ann.node {
                tinct::Annotation::Simple(type_name) => {
                    doc_contract.insert("type".into(), serde_json::json!(type_name));
                }
                tinct::Annotation::PropertyDict(entries) => {
                    let mut fields = serde_json::Map::new();
                    for entry in entries {
                        if let Some(ref key_expr) = entry.node.key {
                            if let tinct::Expr::Str(ref key_name) = key_expr.node {
                                fields.insert(
                                    key_name.clone(),
                                    describe_annotation_value(&entry.node.value.node),
                                );
                            }
                        }
                    }
                    if !fields.is_empty() {
                        doc_contract.insert("fields".into(), serde_json::Value::Object(fields));
                    }
                }
                tinct::Annotation::Annotated(name, _inner) => {
                    doc_contract.insert("type".into(), serde_json::json!(name));
                }
            }
        }

        // Detect schema dicts in the document expressions
        let schema_fields = detect_schema_dict(&doc.node.expressions);
        if !schema_fields.is_empty() {
            has_any_contract = true;
            doc_contract.insert("schema".into(), serde_json::Value::Object(schema_fields));
        }

        // Include doc strings from DocMap for top-level bindings
        let doc_strings = extract_doc_strings_from_doc(&doc.node, &doc_map);
        if !doc_strings.is_empty() {
            has_any_contract = true;
            doc_contract.insert("docs".into(), serde_json::Value::Object(doc_strings));
        }

        if doc_contract.len() > 1 {
            // Has more than just "section"
            contracts.push(serde_json::Value::Object(doc_contract));
        }
    }

    if !has_any_contract {
        if json_mode {
            println!("{{}}");
        } else {
            println!("no input contract");
        }
        return Ok(());
    }

    if json_mode {
        let output = serde_json::json!({ "contracts": contracts });
        let pretty =
            serde_json::to_string_pretty(&output).map_err(|e| format!("JSON error: {e}"))?;
        println!("{pretty}");
    } else {
        // Human-readable output: one line per field, with doc strings
        for contract in &contracts {
            if let Some(section) = contract.get("section") {
                if contracts.len() > 1 {
                    println!("--- section {} ---", section);
                }
            }
            if let Some(type_name) = contract.get("type") {
                println!("  expects: @{}", type_name.as_str().unwrap_or("?"));
            }
            if let Some(fields) = contract.get("fields").and_then(|f| f.as_object()) {
                for (name, constraint) in fields {
                    print!("  {}: {}", name, format_constraint(constraint));
                    // Add doc string if available
                    if let Some(docs) = contract.get("docs").and_then(|d| d.as_object()) {
                        if let Some(doc_str) = docs.get(name).and_then(|v| v.as_str()) {
                            print!(" — {}", doc_str);
                        }
                    }
                    println!();
                }
            }
            if let Some(schema) = contract.get("schema").and_then(|s| s.as_object()) {
                for (name, constraint) in schema {
                    print!("  {}: {}", name, format_constraint(constraint));
                    // Add doc string if available
                    if let Some(docs) = contract.get("docs").and_then(|d| d.as_object()) {
                        if let Some(doc_str) = docs.get(name).and_then(|v| v.as_str()) {
                            print!(" — {}", doc_str);
                        }
                    }
                    println!();
                }
            }
            // If there are doc strings for bindings not in fields/schema, show them
            if let Some(docs) = contract.get("docs").and_then(|d| d.as_object()) {
                let field_names: std::collections::HashSet<&String> = contract
                    .get("fields")
                    .and_then(|f| f.as_object())
                    .map(|o| o.keys().collect())
                    .unwrap_or_default();
                let schema_names: std::collections::HashSet<&String> = contract
                    .get("schema")
                    .and_then(|s| s.as_object())
                    .map(|o| o.keys().collect())
                    .unwrap_or_default();

                for (name, doc_str) in docs {
                    if !field_names.contains(name) && !schema_names.contains(name) {
                        if let Some(doc_val) = doc_str.as_str() {
                            println!("  {} — {}", name, doc_val);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Extract doc strings from a document's top-level bindings.
/// Scans dict expressions in the document for entries that have doc strings in the DocMap.
fn extract_doc_strings_from_doc(
    doc: &tinct::Document,
    doc_map: &std::collections::HashMap<String, String>,
) -> serde_json::Map<String, serde_json::Value> {
    let mut result = serde_json::Map::new();

    for expr in &doc.expressions {
        if let tinct::Expr::Dict(entries) = &expr.node {
            for entry in entries {
                if let Some(ref key_expr) = entry.node.key {
                    // Extract the binding name from the key expression
                    // Keys can be:
                    // - Expr::Str (string literal key)
                    // - Expr::Annotated { name, .. } (annotated binding like name@[...])
                    // - Expr::VarRef (bare identifier key)
                    let name_opt = match &key_expr.node {
                        tinct::Expr::Str(s) => Some(s.as_str()),
                        tinct::Expr::Annotated { name, .. } => Some(name.as_str()),
                        tinct::Expr::VarRef { name, .. } => Some(name.as_str()),
                        _ => None,
                    };

                    if let Some(name) = name_opt {
                        if let Some(doc_str) = doc_map.get(name) {
                            result.insert(name.to_string(), serde_json::json!(doc_str));
                        }
                    }
                }
            }
        }
    }

    result
}

/// Turn an annotation value expression into a JSON description.
fn describe_annotation_value(expr: &tinct::Expr) -> serde_json::Value {
    match expr {
        tinct::Expr::Str(s) => serde_json::json!(s),
        tinct::Expr::Int(n) => serde_json::json!(n),
        tinct::Expr::Float(f) => serde_json::json!(f),
        tinct::Expr::Bool(b) => serde_json::json!(b),
        tinct::Expr::VarRef { name, .. } => serde_json::json!(name),
        _ => serde_json::json!("(complex)"),
    }
}

/// Detect schema dicts in a document's expressions.
///
/// A dict is a schema dict if any of its values is itself a dict containing
/// at least one recognized schema key (type, min, max, min-length, max-length,
/// pattern, required, items, fields, enum).
fn detect_schema_dict(
    expressions: &[std::rc::Rc<tinct::Spanned<tinct::Expr>>],
) -> serde_json::Map<String, serde_json::Value> {
    let mut result = serde_json::Map::new();
    for expr in expressions {
        if let tinct::Expr::Dict(entries) = &expr.node {
            for entry in entries {
                if let Some(ref key_expr) = entry.node.key {
                    if let tinct::Expr::Str(ref field_name) = key_expr.node {
                        // Check if the value is a dict with schema keys
                        if let Some(schema_info) = extract_schema_info(&entry.node.value.node) {
                            result.insert(field_name.clone(), schema_info);
                        }
                    }
                }
            }
        }
    }
    result
}

/// If `expr` is a dict containing at least one recognized schema key, return
/// a JSON object describing the constraints. Otherwise return None.
fn extract_schema_info(expr: &tinct::Expr) -> Option<serde_json::Value> {
    if let tinct::Expr::Dict(entries) = expr {
        let mut info = serde_json::Map::new();
        let mut has_schema_key = false;
        for entry in entries {
            if let Some(ref key_expr) = entry.node.key {
                if let tinct::Expr::Str(ref key_name) = key_expr.node {
                    if SCHEMA_KEYS.contains(&key_name.as_str()) {
                        has_schema_key = true;
                        info.insert(
                            key_name.clone(),
                            describe_annotation_value(&entry.node.value.node),
                        );
                    }
                }
            }
        }
        if has_schema_key {
            return Some(serde_json::Value::Object(info));
        }
    }
    None
}

/// Format a constraint JSON value as a human-readable string.
fn format_constraint(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    let v_str = match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    format!("{k}: {v_str}")
                })
                .collect();
            parts.join(", ")
        }
        other => other.to_string(),
    }
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

        "E090" => {
            "\
E090: Schema validation failed

The $validate builtin found one or more constraint violations when checking
data against a schema. The error message lists each field path and the
specific constraint that was violated.

Schema constraints include:
  - type: expected value type (Int, String, Bool, etc.)
  - min/max: numeric range constraints
  - min-length/max-length: string or sequence length constraints
  - pattern: regex pattern for strings
  - required: field must be present
  - enum: value must be one of the listed options
  - items: schema for sequence elements
  - fields: schema for dict fields

Fix: correct the data to satisfy the schema constraints, or adjust the
schema to match the actual data structure."
        }

        "E099" => {
            "\
E099: Internal error

An unexpected internal condition occurred in the evaluator. This should not
happen in normal use.

Fix: file a bug report with the full error message and the LLT source file
that triggered the error."
        }

        // --- Type checker codes (T000-T004) ---
        "T000" => {
            "\
T000: General type error

A type checking constraint was violated that does not fall into one of the
more specific categories (T001-T004). The error message describes the
specific constraint that failed.

Fix: read the error message to understand the constraint, then adjust the
type annotations or expression structure to satisfy it."
        }

        "T001" => {
            "\
T001: Arity mismatch (type checker)

A function was called with the wrong number of arguments according to its
declared type. For example, calling a Fn@Int [Int String] (which takes 2
arguments) with only 1.

Note: the type checker counts named arguments toward the arity. If a named
argument fills one positional slot, the arity check accounts for it.

Fix: supply the correct number of arguments, or update the function's type
annotation if the arity is intentionally changing."
        }

        "T002" => {
            "\
T002: Undefined variable (type checker)

A variable or type name was referenced in a type expression or value
expression but could not be found in any enclosing scope visible to the
type checker.

Common causes:
  - Sequential bindings are not visible to the type checker by default
    (only letrec dict bindings are in scope at typecheck time).
  - A type alias was referenced before it was defined.
  - A typo in the variable name.

Fix: ensure the variable is defined in the same dict scope (not a
Sequential binding), or check the name for typos."
        }

        "T003" => {
            "\
T003: Cannot unify (type checker)

Two types that were expected to be compatible turned out to be incompatible.
For example, a function declared to return String but whose body produces Int,
or passing a String where an Int parameter is expected.

The error message shows:
  expected `<type>` — the type required by the context
  found `<type>`    — the type that was actually inferred

Fix: make the expression's type match the expected type. Common approaches:
  - Add or remove a type conversion ($to-int, $to-str, etc.)
  - Update the type annotation to match the actual type
  - Fix a logic error that causes the wrong type to be produced."
        }

        "T004" => {
            "\
T004: Type assertion or match coverage failure (type checker)

Either:
  - A match expression does not cover all possible values of its scrutinee
    type (non-exhaustive match), or
  - A type assertion annotation disagrees with the inferred type of the
    annotated expression.

For non-exhaustive match: the error message lists the uncovered patterns.
For type assertion: the message shows the expected and found types.

Fix for non-exhaustive match: add arms to cover the missing patterns, or
add a wildcard arm (_) to handle any remaining cases.
Fix for type assertion: update the annotation to match the runtime type, or
fix the expression to produce the declared type."
        }

        _ => {
            return Err(format!(
                "unknown error code: {code}\n\
                 Run 'tinct explain <code>' with a valid code, e.g. E001 through E099 or T000-T004.\n\
                 Known codes: E001, E002, E010, E011, E020-E024, E030-E036, \
                 E040-E043, E050-E057, E060-E062, E070, E080, E090, E099, \
                 T000, T001, T002, T003, T004."
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

    #[test]
    fn parse_net_cap_entry_cidr_ipv4() {
        use tinct::NetCapEntry;
        let entry = parse_cli_net_cap_entry("192.168.1.0/24").unwrap();
        match entry {
            NetCapEntry::Cidr(net) => {
                assert_eq!(net.to_string(), "192.168.1.0/24");
                assert!(net.contains(&"192.168.1.1".parse::<std::net::IpAddr>().unwrap()));
                assert!(net.contains(&"192.168.1.254".parse::<std::net::IpAddr>().unwrap()));
                assert!(!net.contains(&"192.168.2.1".parse::<std::net::IpAddr>().unwrap()));
            }
            _ => panic!("Expected Cidr variant"),
        }
    }

    #[test]
    fn parse_net_cap_entry_cidr_ipv6() {
        use tinct::NetCapEntry;
        let entry = parse_cli_net_cap_entry("2001:db8::/32").unwrap();
        match entry {
            NetCapEntry::Cidr(net) => {
                assert_eq!(net.to_string(), "2001:db8::/32");
                assert!(net.contains(&"2001:db8::1".parse::<std::net::IpAddr>().unwrap()));
                assert!(net.contains(
                    &"2001:db8:ffff:ffff:ffff:ffff:ffff:ffff"
                        .parse::<std::net::IpAddr>()
                        .unwrap()
                ));
                assert!(!net.contains(&"2001:db9::1".parse::<std::net::IpAddr>().unwrap()));
            }
            _ => panic!("Expected Cidr variant"),
        }
    }

    #[test]
    fn parse_net_cap_entry_cidr_single_host() {
        use tinct::NetCapEntry;
        let entry = parse_cli_net_cap_entry("10.0.0.5/32").unwrap();
        match entry {
            NetCapEntry::Cidr(net) => {
                assert!(net.contains(&"10.0.0.5".parse::<std::net::IpAddr>().unwrap()));
                assert!(!net.contains(&"10.0.0.6".parse::<std::net::IpAddr>().unwrap()));
            }
            _ => panic!("Expected Cidr variant"),
        }
    }

    #[test]
    fn parse_net_cap_entry_cidr_invalid() {
        assert!(parse_cli_net_cap_entry("192.168.1.0/33").is_err());
        assert!(parse_cli_net_cap_entry("not-an-ip/24").is_err());
        assert!(parse_cli_net_cap_entry("192.168.1.0/").is_err());
    }

    #[test]
    fn parse_net_cap_entry_hostname() {
        use tinct::NetCapEntry;
        let entry = parse_cli_net_cap_entry("example.com").unwrap();
        assert!(matches!(entry, NetCapEntry::Hostname(h) if h == "example.com"));
    }

    #[test]
    fn parse_net_cap_entry_hostport() {
        use tinct::NetCapEntry;
        let entry = parse_cli_net_cap_entry("example.com:443").unwrap();
        assert!(matches!(entry, NetCapEntry::HostPort(h, p) if h == "example.com" && p == 443));
    }

    #[test]
    fn parse_net_cap_entry_glob() {
        use tinct::NetCapEntry;
        let entry = parse_cli_net_cap_entry("*.internal").unwrap();
        assert!(matches!(entry, NetCapEntry::HostnameGlob(g) if g == "*.internal"));
    }

    #[test]
    fn parse_net_cap_entry_any() {
        use tinct::NetCapEntry;
        let entry = parse_cli_net_cap_entry("any").unwrap();
        assert!(matches!(entry, NetCapEntry::Any));
    }
}
