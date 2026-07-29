//! LLT command-line tool: parses and evaluates `.llt` files, outputs JSON or LLT display format.

use clap::{Parser, Subcommand, ValueEnum};
use std::io::{self, Read};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tinct::{
    build_core_env, format_type_diagnostic, literate, parse, string_val, unknown_type_val,
    EvalContext, HashableValue, Thunk, Value,
};
// Exit codes for llt eval
const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_TIMEOUT: i32 = 2;
// EXIT_OOM (3, formerly used for soft heap limit) is removed — the soft AtomicI64 budget
// tracker (memory_budget.rs) is deleted. The hard RLIMIT_AS backstop still applies
// but causes abort via handle_alloc_error, not a clean exit.
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
    /// Maximum virtual address space (bytes) the process may use. Enforced via
    /// RLIMIT_AS. Default: 512 MB. Set to 0 to disable. (Unix only)
    #[arg(long, value_name = "BYTES", global = true)]
    max_memory: Option<u64>,

    /// Maximum CPU time (seconds) the process may consume. Enforced via
    /// RLIMIT_CPU. Sends SIGXCPU on soft limit, SIGKILL on hard limit.
    /// Complements --timeout (wall-clock). (Unix only)
    #[arg(long, value_name = "SECONDS", global = true)]
    max_cpu: Option<u64>,

    /// Maximum number of open file descriptors. Enforced via RLIMIT_NOFILE.
    /// Default: 64. Set to 0 to disable. (Unix only)
    #[arg(long, value_name = "COUNT", global = true)]
    max_fds: Option<u64>,

    #[command(subcommand)]
    command: Commands,
}

/// All CLI flags for the `run` / `eval` subcommand.
///
/// Extracted into a separate struct so the `Commands::Run` enum variant can hold
/// `Box<RunArgs>` instead of inline fields, keeping the enum variant within Clippy's
/// large-enum-variant threshold.
#[derive(clap::Args, Debug)]
struct RunArgs {
    /// Disable all filesystem access: suppresses %cwd, %libdir, and any caps injected
    /// via --cap-fs or --cap-file. Scripts that attempt filesystem operations fail.
    /// Use --no-cwd or --no-libdir for fine-grained suppression.
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

    /// Disable environment variable access. $env returns Null for all names.
    #[arg(long)]
    no_env: bool,

    /// Allow $env to read specific environment variable(s) by name (may be repeated).
    /// When any --allow-env flag is present, $env returns Null for unlisted names.
    #[arg(long, value_name = "NAME")]
    allow_env: Vec<String>,

    /// Do not inject `%cwd` DirCap into the root environment.
    /// When set, [open %cwd ...] and [include %cwd ...] fail with undefined variable.
    #[arg(long)]
    no_cwd: bool,

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
    /// Format: NAME=PATH:MODE — binds %NAME to a DirCap. MODE is required.
    /// MODE is one or more of: r (read), w (write), l (list), s (stat).
    /// Example: docs=/tmp/mydocs:rl injects %docs with read+list access.
    #[arg(long, value_name = "NAME=PATH:MODE")]
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
    /// Required to read from stdin. Error if the formatter file does not exist.
    #[arg(short = 'i', long = "input", value_name = "FORMAT")]
    input: Option<String>,

    /// Append an output formatter from stdlib/cli/out/<format>.llt as the final pipeline stage.
    /// Error if the formatter file does not exist.
    #[arg(short = 'o', long = "output", value_name = "FORMAT")]
    output: Option<String>,

    /// Alternative init program to use instead of the embedded stdlib/loader.llt.
    /// The init program receives the same %programs, %args, %cwd, %libdir
    /// as the standard loader.llt. %stdout and %stderr are defined by the init program itself.
    #[clap(long, value_name = "FILE")]
    init: Option<String>,

    /// Write profiling data to a JSON file. Collects span-level timing data during evaluation.
    /// Each thunk materialization produces a span record with source location, timing, parent
    /// attribution, and stall breakdown. Use tinct scripts in scripts/profile/ to analyze.
    #[arg(long, value_name = "FILE")]
    profile: Option<String>,

    /// Input LLT files. Use `-` to read LLT source from stdin.
    /// Multiple files form a pipeline: each file's output becomes % for the next.
    files: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Evaluate an LLT file and output the result.
    #[clap(alias = "eval")]
    Run(Box<RunArgs>),
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
        /// Format: NAME=PATH:MODE — binds %NAME to a DirCap. MODE is required.
        /// MODE is one or more of: r (read), w (write), l (list), s (stat).
        /// Example: docs=/tmp/mydocs:rl injects %docs with read+list access.
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
    /// Extract blocks, evaluate as a pipeline, print the result.
    Eval,
    /// Extract blocks, evaluate each cumulatively, annotate the markdown with results.
    Weave,
}

fn main() {
    // async_rt::block_on uses current_thread Tokio runtime + thread-local LocalSet.
    // This ensures spawn_local tasks (e.g., emit channel drain in formatters) are
    // driven alongside the main future. #[tokio::main]'s multi_thread runtime does
    // NOT drive LocalSet, so spawn_local tasks spawned by [task ...] never execute.
    //
    // process::exit() is called AFTER block_on() returns so that the Tokio runtime
    // is no longer on the stack. Calling process::exit() from inside an async context
    // panics with "Oh no! We never placed the Core back, this is a bug!".
    let exit_code = tinct::async_rt::block_on(async_main());
    std::process::exit(exit_code);
}

async fn async_main() -> i32 {
    // Install ring as the process-level TLS crypto provider.
    // Both ring and aws-lc-rs are compiled in (via quinn+reqwest feature flags);
    // rustls panics at runtime if the process default is ambiguous.
    // quinn already requires ring, so ring is the consistent choice.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install ring as rustls default crypto provider");

    let cli = Cli::parse();

    // Hard RLIMIT_AS backstop: catches anything that bypasses the allocator
    // (direct mmap, stack growth, shared-library mappings).
    #[cfg(unix)]
    if let Err(e) = setup_rlimits(cli.max_memory, cli.max_cpu, cli.max_fds) {
        eprintln!("error: {e}");
        return EXIT_ERROR;
    }

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
            if libc::setrlimit(libc::RLIMIT_STACK, &new_rl) != 0 {
                let err = std::io::Error::last_os_error();
                eprintln!("tinct: warning: failed to raise RLIMIT_STACK: {err}");
            }
        }
    }

    // Materialize is iterative (materialize_rc loop); no large worker stack needed.
    let result = match cli.command {
        Commands::Run(args) => run_eval(*args).await,
        Commands::Hash { file } => run_hash(&file),
        Commands::Fmt {
            check,
            in_place,
            output,
            strict,
            file,
        } => run_fmt(&file, check, in_place, &output, strict).await,
        Commands::Explain { code } => run_explain(&code),
        Commands::Literate {
            mode,
            file,
            no_substitute: _,
            strict,
            in_place,
            verify,
            fail_on_errors,
            cap_fs,
            cap_net,
        } => {
            run_literate(&LiterateConfig {
                file_path: &file,
                mode: &mode,
                strict,
                in_place,
                verify,
                fail_on_errors,
                cap_fs: &cap_fs,
                cap_net: &cap_net,
            })
            .await
        }
    };

    match result {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("{e}");
            EXIT_ERROR
        }
    }
}

/// Open cap_std::fs::Dir entries for the given --cap-fs list.
/// Skips injection when no_fs is true.
/// Returns Vec<(name, Arc<cap_std::fs::Dir>, perms)>.
// AMBIENT-OK: CLI bootstrap — operator-specified --cap-fs paths
fn open_cap_fs_entries(
    cap_fs: &[String],
    no_fs: bool,
) -> Result<Vec<(String, Arc<cap_std::fs::Dir>, tinct::DirPerms)>, String> {
    if no_fs {
        return Ok(Vec::new());
    }

    let parsed_entries = parse_cap_fs_entries(cap_fs)?;
    let mut result = Vec::new();

    for (name, cap_path, perms) in parsed_entries {
        let cap_dir = cap_std::fs::Dir::open_ambient_dir(&cap_path, cap_std::ambient_authority())
            .map_err(|e| {
            format!(
                "--cap-fs: cannot open directory {:?}: {e}",
                cap_path.display()
            )
        })?;
        result.push((name, Arc::new(cap_dir), perms));
    }

    Ok(result)
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

/// Global stop flag for the profile background flush thread.
///
/// Set by BOTH the SIGINT handler and the main thread. The background thread exits
/// its loop on seeing this flag. Whether it calls `_exit(130)` or returns normally
/// depends on PROFILE_SIGINT_EXIT (see below).
/// AtomicBool store/load with Relaxed ordering is async-signal-safe per POSIX.
static PROFILE_FLUSH_STOP: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Set ONLY by the SIGINT handler (not by the main thread).
///
/// When true, the background flush thread calls `_exit(130)` after the final flush
/// instead of returning normally. This is the Ctrl-C path: the main thread may be
/// blocked inside `block_on` and never reach the join point, so the background thread
/// must terminate the process itself after saving the profile.
static PROFILE_SIGINT_EXIT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// SIGINT handler installed when `--profile` is set.
///
/// Sets both PROFILE_FLUSH_STOP and PROFILE_SIGINT_EXIT so the background flush thread
/// performs a final flush and then calls `_exit(130)`. We do NOT re-raise SIGINT or call
/// `_exit` here because the background thread needs up to 1 s to finish writing the file.
#[cfg(unix)]
extern "C" fn sigint_profile_handler(_sig: i32) {
    use std::sync::atomic::Ordering;
    PROFILE_FLUSH_STOP.store(true, Ordering::Relaxed);
    PROFILE_SIGINT_EXIT.store(true, Ordering::Relaxed);
    // The background thread wakes within 1 s, flushes, and calls _exit(130).
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

/// Install the SIGINT handler that triggers profile flushing.
///
/// Only called when `--profile` is set. The handler sets PROFILE_FLUSH_STOP;
/// the background flush thread polls this flag every 1 s and calls `_exit(130)`
/// after the final flush. Without this handler, Ctrl-C kills the process before
/// the background thread can write the profile file.
#[cfg(unix)]
fn install_sigint_profile_handler() -> Result<(), String> {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = sigint_profile_handler as *const () as libc::sighandler_t;
        // No SA_RESTART: we want blocked syscalls (e.g., block_on) to return EINTR
        // so the main thread can propagate the cancellation. The eval loop will see
        // errors or short-circuit and reach the join point.
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);

        if libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut()) != 0 {
            return Err("profiling: failed to install SIGINT handler".to_string());
        }
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
/// Limits are only applied when explicitly requested (`Some(N)` where N > 0).
/// `None` or `Some(0)` disables that particular limit.
/// - `max_memory`: RLIMIT_AS — no default; container/OS is the backstop.
/// - `max_cpu`: RLIMIT_CPU — no default; must be explicitly requested.
/// - `max_fds`: RLIMIT_NOFILE — default 64 (prevents FD exhaustion from crafted
///   $include chains; still leaves room for stdin/stdout/stderr + eval fds).
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
        let ret = unsafe { libc::setrlimit(resource, &rlim) };
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

    // RLIMIT_AS: virtual address space limit. Only applied when explicitly requested.
    // Default is disabled — the container or OS is the resource enforcement layer.
    // Value of 0 means: caller explicitly disabled this limit.
    let memory_limit = max_memory.unwrap_or(0) as libc::rlim_t;
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
    ruleset_created
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

    if s.contains('/') {
        // CIDR range — check before host:port to avoid misinterpreting IPv6
        // addresses (e.g. 2001:db8::/32) as host:port.
        match s.parse::<ipnet::IpNet>() {
            Ok(net) => Ok(NetCapEntry::Cidr(net)),
            Err(e) => Err(format!("--cap-net: invalid CIDR notation '{}': {}", s, e)),
        }
    } else if s.contains('*') {
        // Glob pattern (prefix wildcard only)
        if !s.starts_with("*.") {
            return Err(format!(
                "--cap-net: only prefix wildcards are supported (e.g., '*.internal'), got '{}'",
                s
            ));
        }
        Ok(NetCapEntry::HostnameGlob(s.to_string()))
    } else if let Some((host, port_str)) = s.rsplit_once(':') {
        // host:port format — use rsplit_once so IPv6 addresses without CIDR
        // (e.g. [::1]:8080) split on the last colon.
        if let Ok(port) = port_str.parse::<u16>() {
            Ok(NetCapEntry::HostPort(host.to_string(), port))
        } else {
            // Not a valid port — treat as plain hostname
            Ok(NetCapEntry::Hostname(s.to_string()))
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
                    | "--init"
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

/// Parse --cap-fs entries from CLI arguments into (NAME, PATH, DirPerms) tuples.
/// Returns an error if any entry is malformed or has invalid mode syntax.
fn parse_cap_fs_entries(
    cap_fs: &[String],
) -> Result<Vec<(String, PathBuf, tinct::DirPerms)>, String> {
    use std::path::PathBuf;
    use tinct::DirPerms;

    let mut result = Vec::new();

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

        // Split PATH:MODE on the last colon (rsplit_once handles Windows drive letters)
        let (path_str, mode_str) = match path_and_mode.rsplit_once(':') {
            Some((path, mode)) => (path, Some(mode)),
            None => (path_and_mode, None),
        };

        let cap_path = PathBuf::from(path_str.trim());

        // Parse mode into DirPerms
        let perms = if let Some(mode) = mode_str {
            let mode = mode.trim();
            if mode.is_empty() {
                return Err(format!(
                    "--cap-fs mode string is empty: NAME=PATH: (did you mean NAME=PATH:r?)\nGot: {:?}",
                    cap_fs_entry
                ));
            }
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
                    symlinkable: false,
                    posix_permissions: false,
                    extended_attributes: false,
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
                        "Symlinkable" => perms.symlinkable = true,
                        "PosixPermissions" => perms.posix_permissions = true,
                        "ExtendedAttributes" => perms.extended_attributes = true,
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
                // Letter mode: r/w/a/s/l/y
                let mut perms = DirPerms {
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
                for c in mode.chars() {
                    if let Some(letter_perms) = DirPerms::from_letter(c) {
                        perms = perms.union(&letter_perms);
                    } else {
                        return Err(format!(
                            "--cap-fs: unknown mode letter {:?} (expected r/w/a/s/l/y)",
                            c
                        ));
                    }
                }
                perms
            }
        } else {
            // No mode specified → error (mode is required)
            return Err(format!(
                "--cap-fs requires mode suffix: NAME=PATH:MODE (e.g., mydir=/tmp:rwls)\nGot: {:?}",
                cap_fs_entry
            ));
        };

        result.push((name.to_string(), cap_path, perms));
    }

    Ok(result)
}

/// Spawn the background profile flush thread.
///
/// The thread loops indefinitely, sleeping in 1-second intervals. Every `flush_interval`
/// complete intervals it calls `drain_new()` on the collector to get spans added since the
/// last flush, serializes each span as a single-line tinct dict (LLT-stream), and appends
/// them to the shared `BufWriter<File>`. When `PROFILE_FLUSH_STOP` is set (by the SIGINT
/// handler or the main thread after eval), the thread performs one final drain and exits.
///
/// Uses `PROFILE_FLUSH_STOP` (global AtomicBool) rather than an `Arc<AtomicBool>` so
/// the SIGINT signal handler (an `extern "C" fn`) can set it without needing a closure.
///
/// Returns the `JoinHandle` so the main thread can join and ensure the final flush
/// completes before `run_eval` returns.
///
/// # Manual test procedure
///
/// 1. Run a long-evaluating script with `--profile /tmp/spans.llt-stream`:
///    `tinct run --profile /tmp/spans.llt-stream heavy.llt`
/// 2. After ~10 s (first flush interval), `wc -l /tmp/spans.llt-stream` should show growing lines.
///    Use `tail -f /tmp/spans.llt-stream` to watch spans arrive.
/// 3. Send Ctrl-C. Within ~1 s, the file should contain all spans written so far.
/// 4. Let the script complete normally. The final drain in `run_eval` appends remaining spans.
// AMBIENT-OK: Profile output file is user-specified via --profile (CLI operator choice).
// Writing to an operator-specified output path is a legitimate ambient write — it is
// not reading untrusted file content. The background thread cannot hold a cap_std Dir
// because Dir is !Send and the thread is a plain OS thread.
fn spawn_profile_flush_thread(
    collector: Arc<std::sync::Mutex<tinct::profiling::ProfilingCollector>>,
    file: Arc<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>,
    flush_interval_secs: u64,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::Write;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        // Count 1-second ticks to know when a full flush interval has elapsed.
        let mut ticks: u64 = 0;

        loop {
            std::thread::sleep(Duration::from_secs(1));
            ticks += 1;

            let should_stop = PROFILE_FLUSH_STOP.load(Ordering::Relaxed);

            // Flush every `flush_interval_secs` ticks, or on stop.
            if should_stop || ticks >= flush_interval_secs {
                ticks = 0;

                // Distinguish SIGINT-stop (background thread must call _exit) from
                // main-thread-stop (background thread should return normally so join() works).
                // PROFILE_SIGINT_EXIT is set only by the signal handler; PROFILE_FLUSH_STOP
                // is set by both the signal handler and the main thread.
                let sigint_exit = PROFILE_SIGINT_EXIT.load(Ordering::Relaxed);

                // Drain new spans while holding the collector lock as briefly as possible.
                let new_spans = {
                    match collector.lock() {
                        Ok(mut guard) => guard.drain_new(),
                        Err(_) => {
                            // Lock poisoned — collector is inconsistent; nothing safe to flush.
                            if should_stop && sigint_exit {
                                unsafe { libc::_exit(130) };
                            }
                            return;
                        }
                    }
                };

                // Serialize each new span as one LLT-stream line and append to the file.
                if !new_spans.is_empty() {
                    match file.lock() {
                        Ok(mut file_guard) => {
                            let mut write_ok = true;
                            for span in &new_spans {
                                let line = span.to_tinct_line();
                                if let Err(e) = writeln!(file_guard, "{}", line) {
                                    eprintln!("profiling: background flush write error: {e}");
                                    write_ok = false;
                                    break;
                                }
                            }
                            if write_ok {
                                if let Err(e) = file_guard.flush() {
                                    eprintln!("profiling: background flush error: {e}");
                                }
                            }
                        }
                        Err(_) => {
                            // File mutex is poisoned — the write path is broken.
                            // Stop the background flush thread rather than continuing to poll.
                            eprintln!("profiling: background flush file lock poisoned — stopping flush thread");
                            if sigint_exit {
                                unsafe { libc::_exit(130) };
                            }
                            return;
                        }
                    }
                }

                if should_stop {
                    if sigint_exit {
                        // SIGINT path: main thread may be blocked in block_on and never
                        // reach join(). We must terminate the process ourselves.
                        // Exit code 130 = 128 + SIGINT (conventional Ctrl-C shell code).
                        unsafe { libc::_exit(130) };
                    }
                    // Normal-stop path (main thread stopped us): return so join() succeeds.
                    return;
                }
            }
        }
    })
}

// CLI entrypoint with all flags
// AMBIENT-OK: CLI bootstrap — operator-specified file paths and capability directories
async fn run_eval(args: RunArgs) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let no_landlock = args.no_landlock;
    let RunArgs {
        files: file_paths_owned,
        no_fs,
        require_integrity,
        strict,
        timeout,
        no_landlock: _,
        no_env,
        allow_env,
        no_cwd,
        no_libdir,
        libdir_path,
        cap_fs,
        cap_net,
        no_cap_clock,
        cap_clock_fixed,
        cap_file,
        init,
        expr,
        input,
        output,
        profile: profile_owned,
    } = args;
    let file_paths = &file_paths_owned;
    let profile = profile_owned.as_deref();
    // Build the interleaved list of user pipeline stages: files and -e expressions in CLI order.
    // Clap doesn't preserve mixed positional/flag order, so we reconstruct it by parsing raw args.
    //
    // NOTE: The -i/-o formatter stages are NO LONGER added here Rust-side.
    // loader.llt (dict 3) constructs the formatter ProgramItem.File from %args.output and passes
    // it separately to cli-pipeline, which runs it last. See doc/whatif/type-foundations.md §src/main.rs.
    let interleaved_stages = interleave_files_and_exprs(file_paths, &expr);

    // Validate formatter names early (before any file opens) so errors are reported cleanly.
    if let Some(ref input_format) = input {
        if !input_format
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-')
        {
            return Err(format!(
                "--input: invalid formatter name {:?} (only alphanumeric and '-' allowed)",
                input_format
            ));
        }
    }
    if let Some(ref output_format) = output {
        if !output_format
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-')
        {
            return Err(format!(
                "--output: invalid formatter name {:?} (only alphanumeric and '-' allowed)",
                output_format
            ));
        }
    }

    // ── Pre-open all file handles before Landlock fires ────────────────────────
    //
    // File handles opened here remain valid after Landlock restricts future open() calls.
    // Each file path gets an absolute path (for error reporting) and a readable handle
    // (opened via open_ambient_dir). Expressions (-e) don't need handles.
    //
    // %programs entries (ProgramItem variants) are built from these pre-opened handles.
    // The file index in %programs matches CLI argument order.

    /// A pre-opened file handle and its resolved absolute path.
    // AMBIENT-OK: CLI bootstrap — operator-specified file paths. cap_std is not available
    // pre-Landlock because Landlock has not yet been applied; the operator-controlled paths
    // have not yet been converted to capability-safe Dir handles.
    struct PreOpenedFile {
        abs_path: String,
        handle: std::fs::File,
    }

    // For -i input formatter: validate it exists and record its path.
    // The formatter itself is passed as %args.input to loader.llt (dict 3 builds the ProgramItem).
    // We don't need to pre-open -i/-o formatters: they're in %libdir which is always pre-opened.

    // Pre-open each user file (positional args only; -e expressions have no file).
    let mut pre_opened_files: Vec<(String, PreOpenedFile)> = Vec::new();
    for stage in &interleaved_stages {
        if let PipelineStage::File(file_path) = stage {
            if file_path == "-" {
                // stdin: no pre-open needed; loader.llt reads from %stdin handle.
                continue;
            }
            // Resolve to absolute path for error reporting and %include-dir computation.
            // AMBIENT-OK: CLI bootstrap — reading cwd before Landlock is applied.
            let abs_path = {
                let p = std::path::Path::new(file_path.as_str());
                if p.is_absolute() {
                    file_path.clone()
                } else {
                    std::env::current_dir()
                        .map_err(|e| format!("cannot determine working directory: {e}"))?
                        .join(p)
                        .to_str()
                        .ok_or_else(|| format!("file path is not valid UTF-8: {file_path}"))?
                        .to_string()
                }
            };
            // Open file for reading. Must happen before Landlock.
            // AMBIENT-OK: CLI bootstrap — operator-specified file path opened pre-Landlock.
            let file_handle = std::fs::File::open(file_path.as_str())
                .map_err(|e| format!("cannot open {:?}: {e}", file_path))?;
            pre_opened_files.push((
                file_path.clone(),
                PreOpenedFile {
                    abs_path,
                    handle: file_handle,
                },
            ));
        }
    }
    // Pre-read --init file before Landlock fires (same reason user files are pre-opened).
    // Landlock restricts future open() calls; reading the init source here avoids
    // being blocked by the filesystem ACL if --cap-fs is also specified.
    // AMBIENT-OK: CLI bootstrap — operator-specified --init file path, pre-Landlock.
    let init_source_owned: Option<String> = if let Some(ref path) = init {
        let source = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read --init file '{}': {e}", path))?;
        Some(source)
    } else {
        None
    };

    // Install timeout handler if requested (must happen before evaluation)
    if let Some(ref duration) = timeout {
        #[cfg(unix)]
        {
            install_timeout(duration)?;
        }
        #[cfg(not(unix))]
        {
            return Err("--timeout is only supported on Unix platforms".to_string());
        }
    }

    // Resource limits are now applied globally in main() before subcommand dispatch.

    // Build a fresh core env seeded with only the core Rust builtins.
    // Loader.llt will be evaluated exactly once via run_loader_pipeline below,
    // after all capability thunks have been injected into this env.
    let env = build_core_env();

    // Env is type-metadata only; runtime thunks go into the FlatEnv arena.
    // Collect (name, thunk) cap entries here; inject into arena after eval_ctx is created.
    // Slot names are registered in env immediately (for the resolver's De Bruijn coordinates);
    // the matching thunks are appended to FlatEnv[0] in the same order after construction.
    let mut deferred_cap_thunks: Vec<(String, Arc<tinct::Thunk>)> = Vec::new();

    // Apply Landlock filesystem ACL enforcement (Linux only, defense-in-depth).
    // Auto-triggered when --cap-fs entries are present (unless --no-landlock is set).
    // Derives Landlock roots from the --cap-fs directory paths.
    //
    // File handles for user files are pre-opened above (pre_opened_files) and remain
    // valid after Landlock fires. Landlock restricts future open() calls; existing FDs
    // are unaffected.
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

        // Collect the canonical parent directories of each pre-opened input file.
        // Expressions (-e) don't need Landlock readable paths (no file access).
        let mut extra_readable: Vec<PathBuf> = Vec::new();
        for (_, pf) in &pre_opened_files {
            let path = std::path::Path::new(&pf.abs_path);
            let dir = match path.parent().filter(|d| !d.as_os_str().is_empty()) {
                Some(d) => d.to_path_buf(),
                None => std::env::current_dir()
                    .map_err(|e| format!("cannot determine working directory: {e}"))?,
            };
            match dir.canonicalize() {
                Ok(canon) => extra_readable.push(canon),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Directory was removed between open and canonicalize — skip it.
                }
                Err(e) => {
                    return Err(format!(
                        "cannot canonicalize input file directory '{}': {e}",
                        dir.display()
                    ));
                }
            }
        }

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

    // Install seccomp-bpf network and process sandbox (Linux only).
    // Applied after Landlock so that both kernel-level defenses are active before
    // eval. Gracefully degrades on unsupported kernels (prints warning, continues).
    // Network syscalls are allowed when --cap-net is present (explicit network authority).
    #[cfg(target_os = "linux")]
    {
        let allow_network = !cap_net.is_empty();
        if let Err(e) = setup_seccomp(allow_network) {
            eprintln!("warning: seccomp sandbox not active: {e}");
        }
    }

    // Inject `%cwd` DirCap into the root environment (unless --no-cwd is set).
    // `%cwd` is the process working directory (where `tinct` was invoked from).
    // This allows `[open %cwd "file.txt"]` to access files relative to the invocation directory.
    // --no-cwd enforcement: when the flag is set, `%cwd` is NOT injected, so
    // any reference to `%cwd` in the program will fail with "undefined variable".
    if !no_cwd && !no_fs {
        use tinct::Value;
        // AMBIENT-OK: process CWD at startup
        let cwd_path = std::env::current_dir()
            .map_err(|e| format!("cannot determine working directory for %cwd: {e}"))?;
        let cwd_dir = cap_std::fs::Dir::open_ambient_dir(&cwd_path, cap_std::ambient_authority())
            .map_err(|e| format!("cannot open %cwd directory: {e}"))?;
        let cwd_value = Value::DirCap {
            dir: cwd_dir,
            perms: tinct::DirPerms::full(),
            type_val: tinct::unknown_type_val(),
        };
        let cwd_thunk = Arc::new(tinct::Thunk::value(cwd_value, tinct::rust_span!()));
        env.write()
            .unwrap()
            .insert_slot_name_only("%cwd".to_string());
        deferred_cap_thunks.push(("%cwd".to_string(), cwd_thunk));
    }

    // Inject %stdin as a Value::File wrapping the real stdin file descriptor when -i is specified.
    // Input formatters (cli/in/*.llt) read from %stdin via `lines %stdin` → `read-all` → `read-chunk`.
    // %stdin is only injected when -i is provided: programs that don't need stdin must not acquire it.
    // AMBIENT-OK: CLI bootstrap — stdin fd (0) is a process-level resource granted by the OS.
    if input.is_some() {
        #[cfg(unix)]
        {
            use std::os::unix::io::FromRawFd;
            use std::sync::Mutex;
            // SAFETY: fd 0 is stdin, always valid at process startup.
            // We duplicate it so the Value::File owns an independent fd and closing it
            // does not close the process's stdin for other users (e.g. the shell).
            let stdin_owned: std::fs::File = unsafe { std::fs::File::from_raw_fd(0) };
            let stdin_dup = stdin_owned
                .try_clone()
                .map_err(|e| format!("cannot duplicate stdin fd for %stdin: {e}"))?;
            // Prevent the original from being closed when stdin_owned drops.
            std::mem::forget(stdin_owned);
            let cap_file = cap_std::fs::File::from_std(stdin_dup);
            let stdin_value = Value::File {
                inner: Arc::new(Mutex::new(cap_file)),
                type_val: tinct::unknown_type_val(),
            };
            let stdin_thunk = Arc::new(tinct::Thunk::value(stdin_value, tinct::rust_span!()));
            env.write()
                .unwrap()
                .insert_slot_name_only("%stdin".to_string());
            deferred_cap_thunks.push(("%stdin".to_string(), stdin_thunk));
        }
        #[cfg(not(unix))]
        {
            // On non-Unix platforms, inject %stdin as an empty dict so programs compile
            // and fail gracefully at runtime when attempting to read, rather than failing
            // at resolve time with "undefined variable: %stdin".
            let stdin_value = Value::Dict {
                entries: indexmap::IndexMap::new(),
                type_val: tinct::unknown_type_val(),
            };
            let stdin_thunk = Arc::new(tinct::Thunk::value(stdin_value, tinct::rust_span!()));
            env.write()
                .unwrap()
                .insert_slot_name_only("%stdin".to_string());
            deferred_cap_thunks.push(("%stdin".to_string(), stdin_thunk));
        }
    }

    // NOTE: %stdout and %stderr are NOT injected here.
    // They are defined as nominal type values (Stdout.Stdout, Stderr.Stderr) in loader.llt
    // Dict 2. Writable instances in prelude.llt dispatch to builtin-write-stdout/stderr.

    // Inject `%libdir` DirCap for the stdlib directory (unless --no-libdir is set).
    // --no-libdir enforcement: when the flag is set, `%libdir` is NOT injected, so
    // any reference to `%libdir` in the program will fail with "undefined variable".
    // Phase 1: resolve %libdir from the binary's location, --libdir-path override, or a well-known relative path.
    // If resolution fails, %libdir is not injected (stdlib is embedded at compile time anyway).
    // Inject %libdir when: not explicitly suppressed (--no-libdir), AND either:
    //   - filesystem access is enabled (!no_fs), OR
    //   - a formatter is in use: there is always a formatter (none.llt by default, or -o explicit).
    //     Formatters are system components that need %libdir to load stdlib codecs
    //     (e.g., codecs/json.llt). This is safe because %libdir gives read-only access
    //     to the stdlib directory only.
    // Since none.llt is now always appended when no -o is given, there is always a formatter.
    let has_formatter = true;
    let resolved_libdir_path: Option<std::path::PathBuf> =
        if !no_libdir && (!no_fs || has_formatter) {
            resolve_libdir_path(libdir_path.as_deref())
        } else {
            None
        };
    // libdir_rc_for_ctx: the same Dir is shared with the EvalContext so that
    // the self-hosted `include` (prelude.llt) can inject `%libdir` into nested
    // includes without calling open_ambient_dir again. None when --no-libdir is set
    // and no output formatter is requested.
    let mut libdir_rc_for_ctx: Option<Arc<cap_std::fs::Dir>> = None;
    if !no_libdir && (!no_fs || has_formatter) {
        use tinct::Value;
        if let Some(ref path) = resolved_libdir_path {
            if let Ok(libdir_std) =
                cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())
            {
                let libdir_arc = Arc::new(libdir_std);
                // Clone the Dir for the DirCap value (now owned)
                let libdir_dir_for_cap = libdir_arc.open_dir(".").expect("failed to dup libdir");
                let libdir_value = Value::DirCap {
                    dir: libdir_dir_for_cap,
                    perms: tinct::DirPerms::full(),
                    type_val: tinct::unknown_type_val(),
                };
                let libdir_thunk = Arc::new(tinct::Thunk::value(libdir_value, tinct::rust_span!()));
                env.write()
                    .unwrap()
                    .insert_slot_name_only("%libdir".to_string());
                deferred_cap_thunks.push(("%libdir".to_string(), libdir_thunk));
                libdir_rc_for_ctx = Some(libdir_arc);
            }
            // If the dir can't be opened, silently skip — stdlib is embedded anyway.
        }
    }

    // Inject --cap-fs NAME=PATH[:MODE] entries into the root environment as `%NAME`.
    // The `%` prefix makes injected caps visually distinct from user-defined variables.
    // MODE syntax: r/w/a/s/l letters or [Cap1 Cap2 ...] extended form.
    // --no-fs suppresses all cap-fs injection: operator-specified caps are not available
    // to user code when filesystem access is globally disabled.
    {
        use tinct::Value;
        let cap_entries = open_cap_fs_entries(&cap_fs, no_fs)?;
        for (name, cap_dir_arc, perms) in cap_entries {
            // Clone the Arc to get an independent owned Dir for the DirCap value
            let dir_for_cap = cap_dir_arc.open_dir(".").expect("failed to dup cap dir");
            let cap_value = Value::DirCap {
                dir: dir_for_cap,
                perms,
                type_val: tinct::unknown_type_val(),
            };
            let cap_thunk = Arc::new(tinct::Thunk::value(cap_value, tinct::rust_span!()));
            // Inject as `%NAME` (auto-prefix %).
            let scoped_name = if name.starts_with('%') {
                name.to_string()
            } else {
                format!("%{name}")
            };
            env.write()
                .unwrap()
                .insert_slot_name_only(scoped_name.clone());
            deferred_cap_thunks.push((scoped_name, cap_thunk));
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
            let cap_value = Value::NetCap {
                entries: Arc::new(entries),
                type_val: tinct::unknown_type_val(),
            };
            let cap_thunk = Arc::new(tinct::Thunk::value(cap_value, tinct::rust_span!()));
            env.write().unwrap().insert_slot_name_only(name.clone());
            deferred_cap_thunks.push((name, cap_thunk));
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
            Value::ClockCap {
                inner: Arc::new(ClockCapInner::Fixed(nanos)),
                type_val: tinct::unknown_type_val(),
            }
        } else {
            // Default: real system clock
            Value::ClockCap {
                inner: Arc::new(ClockCapInner::Real),
                type_val: tinct::unknown_type_val(),
            }
        };

        let cap_thunk = Arc::new(tinct::Thunk::value(cap_value, tinct::rust_span!()));
        env.write()
            .unwrap()
            .insert_slot_name_only("%clock".to_string());
        deferred_cap_thunks.push(("%clock".to_string(), cap_thunk));
    }

    // Inject --cap-file NAME=PATH[:MODE] entries into the root environment as `%NAME`.
    // --cap-file injects a DirCap narrowed to the parent directory of PATH, granting the
    // specified permissions (r/w/a) on that directory. This gives tinct programs scoped
    // directory access for the single file path specified, using the DirCap mechanism.
    // --no-fs suppresses all cap-file entries (filesystem access is blocked globally).
    // AMBIENT-OK: CLI bootstrap — operator-specified file paths via --cap-file.
    if !no_fs {
        use tinct::DirPerms;
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
            // If no ':', mode defaults to "r" (readable).
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
            let (readable, writable, appendable, _binary) = if let Some(mode) = mode_str {
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
                // No mode specified → r (readable)
                (true, false, false, false)
            };

            // Validate: at least one of readable/writable/appendable must be set
            if !readable && !writable && !appendable {
                return Err(format!(
                    "--cap-file: mode must specify at least one of Readable, Writable, or Appendable in {:?}",
                    cap_file_entry
                ));
            }

            // Resolve the file path and open its parent directory as a DirCap.
            // --cap-file grants access to the parent directory (narrowed to the specified
            // permissions), not a single file — DirCap is the capability unit in tinct.
            // Programs use `[open %cap-name "filename" mode]` to access the file within it.
            let file_path = std::path::Path::new(path_str);
            let parent_dir = file_path.parent().unwrap_or(std::path::Path::new("."));
            // Resolve to an absolute path for clarity and reproducibility.
            let abs_parent = if parent_dir.as_os_str().is_empty() {
                std::path::Path::new(".").to_path_buf()
            } else {
                parent_dir.to_path_buf()
            };

            let cap_dir =
                cap_std::fs::Dir::open_ambient_dir(&abs_parent, cap_std::ambient_authority())
                    .map_err(|e| {
                        format!(
                            "--cap-file: cannot open parent directory {:?} for {:?}: {e}",
                            abs_parent, cap_file_entry
                        )
                    })?;

            let perms = DirPerms {
                readable,
                statable: readable,
                listable: readable,
                writable,
                appendable,
                deletable: writable,
                renameable: writable,
                symlinkable: false,
                posix_permissions: false,
                extended_attributes: false,
            };

            let cap_value = Value::DirCap {
                dir: cap_dir,
                perms,
                type_val: tinct::unknown_type_val(),
            };
            let cap_thunk = Arc::new(tinct::Thunk::value(cap_value, tinct::rust_span!()));
            // Inject as `%NAME` (auto-prefix %).
            let scoped_name = if name.starts_with('%') {
                name.to_string()
            } else {
                format!("%{name}")
            };
            env.write()
                .unwrap()
                .insert_slot_name_only(scoped_name.clone());
            deferred_cap_thunks.push((scoped_name, cap_thunk));
        }
    }

    // Determine env_allowed based on CLI flags.
    // --no-env and --allow-env enforcement: the `env` prelude function uses builtin-env-has?
    // to check this field before calling builtin-env, returning Absent.Absent for missing vars.
    // None = unrestricted, Some(empty) = all denied (--no-env), Some(set) = only those allowed
    let env_allowed = if no_env {
        Some(std::collections::HashSet::new()) // empty set = all denied
    } else if !allow_env.is_empty() {
        Some(allow_env.into_iter().collect()) // specific allowlist
    } else {
        None // unrestricted
    };

    // Arena sharing invariant: all stages in the pipeline share the same ScopeArena so that
    // ThunkIds allocated for %programs entries remain valid throughout evaluation.
    // The eval_ctx created below owns the arena; fallback pipeline stages derive from it.

    // Initialize profiling collector if --profile is set, and spawn the background
    // flush thread + install the SIGINT handler for fault-tolerant profile writing.
    //
    // The file is opened ONCE in truncate mode at startup. The background flush thread and
    // the final flush path share the file via Arc<Mutex<BufWriter<File>>>. Each flush writes
    // only new spans (via drain_new()) as LLT-stream lines — one tinct dict per line, no wrapping
    // sequence. `tail -f spans.llt-stream` works during long evaluations.
    //
    // IMPORTANT: PROFILE_FLUSH_STOP is a global AtomicBool. If two concurrent
    // `tinct run --profile` invocations exist in the same process (not a supported
    // configuration), the stop flag would be shared. This is acceptable because the
    // flag is only used for graceful shutdown.
    let profiling_collector = profile.map(|_| {
        Arc::new(std::sync::Mutex::new(
            tinct::profiling::ProfilingCollector::new(),
        ))
    });

    // Shared file writer: opened once at startup, used by both the background thread and
    // the final flush path. None when --profile is not set.
    // AMBIENT-OK: Profile output file is user-specified via --profile (CLI operator choice).
    let profile_file: Option<Arc<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>> =
        if let Some(profile_path) = profile {
            match std::fs::File::create(profile_path) {
                Ok(f) => Some(Arc::new(std::sync::Mutex::new(std::io::BufWriter::new(f)))),
                Err(e) => {
                    eprintln!("profiling: cannot open profile file {profile_path}: {e}");
                    None
                }
            }
        } else {
            None
        };

    // Spawn background flush thread and install SIGINT handler when --profile is set.
    // flush_thread_handle is Some(_) iff profiling_collector is Some(_).
    let flush_thread_handle: Option<std::thread::JoinHandle<()>> =
        if let (Some(ref collector), Some(ref pfile)) = (&profiling_collector, &profile_file) {
            // Reset global stop flags (in case a previous run in the same process set them).
            PROFILE_FLUSH_STOP.store(false, std::sync::atomic::Ordering::Relaxed);
            PROFILE_SIGINT_EXIT.store(false, std::sync::atomic::Ordering::Relaxed);

            // Install SIGINT handler so Ctrl-C triggers a final profile flush before exit.
            #[cfg(unix)]
            if let Err(e) = install_sigint_profile_handler() {
                eprintln!("warning: {e}");
            }

            Some(spawn_profile_flush_thread(
                Arc::clone(collector),
                Arc::clone(pfile),
                10, // flush every 10 seconds
            ))
        } else {
            None
        };

    // ── Build %programs Dict ───────────────────────────────────────────────────
    //
    // Integer-keyed Dict of ProgramItem variants, one per CLI stage in order:
    //   ProgramItem.File { path: String, handle: Handle } — for file arguments
    //   ProgramItem.Expr { src: String } — for -e inline expressions
    //
    // File handles stored in ProgramItem.File are the pre-opened handles from above.
    // Landlock is now active; any future open() outside the allowed directories is blocked.
    //
    // The formatter (from -i/-o flags or the default none.llt) is NOT included in %programs.
    // loader.llt dict 3 constructs the formatter ProgramItem.File from %args.output and passes
    // it separately to cli-pipeline as the final stage.
    //
    // Bootstrapping contract: Rust uses qualified tag names "ProgramItem.File" /
    // "ProgramItem.Expr" — the same tags declared in loader.llt dict 2. This is the
    // one Rust↔tinct bootstrap contract for %programs (see doc/whatif/type-foundations.md).

    // Create the base eval context (owns the ScopeArena used by all %programs thunks).
    let eval_ctx = {
        let mut ctx = EvalContext::new_with_options(require_integrity, env_allowed.clone());
        if let Some(ref collector) = profiling_collector {
            Arc::get_mut(&mut ctx).unwrap().profiling = Some(Arc::clone(collector));
        }
        if let Some(ref libdir_rc) = libdir_rc_for_ctx {
            ctx.set_libdir_dir(Arc::clone(libdir_rc));
        }
        // Initialize TypeContext so loader.llt can call [builtin-get-type-context].
        // tycon_env seeded with builtin_core TyCons (Program, DirCap, etc.) so runtime
        // value_matches_type checks have definition spans for clear error messages.
        // Accumulated further by builtin-typecheck-doc calls during loading.
        let (core_type_env, core_tycon_env) = tinct::build_builtin_core_envs().await;
        ctx.init_type_context(tinct::TypeContextData {
            inference_env: core_type_env,
            tycon_env: core_tycon_env,
            type_stage_scope: Vec::new(),
            type_diagnostics: Vec::new(),
        });
        ctx
    };

    // Inject deferred cap thunks into the root scope, carrying the capability name on the span.
    // The name on the thunk's span is how the resolver frame (built from root_group_resolver_map())
    // assigns LGM(slot) addresses. Ordering MUST be consistent: capabilities are
    // appended to root_group after the builtin slots, in the order they are added here.
    // The resolver reads the same ordering from root_group_resolver_map() later.
    let mut capabilities: Vec<(String, Arc<tinct::Thunk>)> = Vec::new();
    for (name, thunk) in deferred_cap_thunks {
        let span = thunk
            .definition_span()
            .with_name(std::sync::Arc::from(name.as_str()));
        let named_thunk = match thunk.peek_result() {
            Some(Ok(val)) => std::sync::Arc::new(tinct::Thunk::value(val.clone(), span)),
            Some(Err(e)) => {
                return Err(format!(
                    "capability '{}' thunk settled with error: {}",
                    name, e
                ))
            }
            None => thunk, // not yet settled — keep original (name from span)
        };
        capabilities.push((name, named_thunk));
    }

    // Helper: create a materialized thunk (Arc<Thunk>) for a value.
    // Used to build Value::Dict entries (which now store Arc<Thunk> directly).
    let mk_thunk = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, tinct::rust_span!())) };

    // Helper: create a materialized thunk for a value.
    let alloc_val = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, tinct::rust_span!())) };

    // Build %programs as an integer-keyed Value::Dict.
    // Each entry is a Value::Variant (ProgramItem.File or ProgramItem.Expr) Arc<Thunk>.
    //
    // Value::Dict stores Arc<Thunk> directly.
    // All thunks allocated here use the eval_ctx arena created above.
    let programs_dict: Value = {
        use indexmap::IndexMap;
        let mut pre_open_iter = pre_opened_files.into_iter();
        let mut dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();

        for (index, stage) in interleaved_stages.iter().enumerate() {
            let key = HashableValue::Int(index as i64);
            let item_value = match stage {
                PipelineStage::File(file_path) => {
                    if file_path == "-" {
                        // stdin: ProgramItem.File with path="-", no handle.
                        // loader.llt eval-file checks path=="-" and reads from %stdin.
                        let mut payload_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                        payload_dict
                            .insert(HashableValue::Str("path".into()), mk_thunk(string_val("-")));
                        let payload_id = alloc_val(Value::Dict {
                            entries: payload_dict,
                            type_val: tinct::unknown_type_val(),
                        });
                        Value::Variant {
                            type_val: unknown_type_val(),
                            ctor: Arc::from("ProgramItem.File"),
                            payload: Some(payload_id),
                        }
                    } else {
                        // Regular file: consume the next pre-opened handle.
                        let pf = pre_open_iter
                            .next()
                            .expect("pre_opened_files count must match non-stdin File stages");
                        let abs_path = pf.1.abs_path;
                        let raw_handle = pf.1.handle;

                        // Wrap the raw std::fs::File as a Value::File (thin OS primitive).
                        // cap_std::fs::File::from_std() wraps a std::fs::File without ambient authority.
                        // AMBIENT-OK: this file was opened before Landlock activation (pre_opened_files).
                        use std::sync::Mutex;
                        let cap_file = cap_std::fs::File::from_std(raw_handle);
                        let handle_value = Value::File {
                            inner: Arc::new(Mutex::new(cap_file)),
                            type_val: tinct::unknown_type_val(),
                        };

                        // Build payload dict: { path: String, handle: Handle }
                        let mut payload_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                        payload_dict.insert(
                            HashableValue::Str("path".into()),
                            mk_thunk(string_val(&abs_path)),
                        );
                        payload_dict
                            .insert(HashableValue::Str("handle".into()), mk_thunk(handle_value));
                        let payload_id = alloc_val(Value::Dict {
                            entries: payload_dict,
                            type_val: tinct::unknown_type_val(),
                        });
                        Value::Variant {
                            type_val: unknown_type_val(),
                            ctor: Arc::from("ProgramItem.File"),
                            payload: Some(payload_id),
                        }
                    }
                }
                PipelineStage::Expr(expression) => {
                    // ProgramItem.Expr { src: String }
                    let mut payload_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                    payload_dict.insert(
                        HashableValue::Str("src".into()),
                        mk_thunk(string_val(expression)),
                    );
                    let payload_id = alloc_val(Value::Dict {
                        entries: payload_dict,
                        type_val: tinct::unknown_type_val(),
                    });
                    Value::Variant {
                        type_val: unknown_type_val(),
                        ctor: Arc::from("ProgramItem.Expr"),
                        payload: Some(payload_id),
                    }
                }
            };
            dict.insert(key, mk_thunk(item_value));
        }
        Value::Dict {
            entries: dict,
            type_val: tinct::unknown_type_val(),
        }
    };

    // ── Build %args Dict ──────────────────────────────────────────────────────
    //
    // Dict with parsed CLI flags. loader.llt reads %args.output to select the formatter
    // and %args.strict to decide whether type errors are fatal.
    //
    // %args.input: name of the -i input formatter (or "" if not specified).
    // The -i formatter is NOT pre-appended to %programs Rust-side. Instead, loader.llt
    // dict 3 reads %args.input to construct the input ProgramItem.File if non-empty.
    let args_dict: Value = {
        use indexmap::IndexMap;
        let mut dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();

        // output: name of the -o formatter (default "none").
        dict.insert(
            HashableValue::Str("output".into()),
            mk_thunk(string_val(output.as_deref().unwrap_or("none"))),
        );

        // input: name of the -i formatter (default "" = no input formatter).
        dict.insert(
            HashableValue::Str("input".into()),
            mk_thunk(string_val(input.as_deref().unwrap_or(""))),
        );

        // strict: whether type errors are fatal.
        dict.insert(
            HashableValue::Str("strict".into()),
            mk_thunk(Value::Int {
                n: if strict { 1 } else { 0 },
                type_val: unknown_type_val(),
            }),
        );

        Value::Dict {
            entries: dict,
            type_val: tinct::unknown_type_val(),
        }
    };

    // Inject %programs and %args into the capabilities list so they appear in root_group
    // alongside the other capabilities (%cwd, %libdir, %clock, etc.).
    // Ordering: %programs and %args are appended last, matching the resolver seed ordering.
    {
        let programs_thunk = std::sync::Arc::new(tinct::Thunk::value(
            programs_dict,
            tinct::rust_span!().with_name(std::sync::Arc::from("%programs")),
        ));
        capabilities.push(("%programs".to_string(), programs_thunk));
        let args_thunk = std::sync::Arc::new(tinct::Thunk::value(
            args_dict,
            tinct::rust_span!().with_name(std::sync::Arc::from("%args")),
        ));
        capabilities.push(("%args".to_string(), args_thunk));
    }

    // Attach the complete capability list to the eval context.
    // All capability thunks (%cwd, %libdir, %clock, --cap-fs / --cap-net, %programs, %args)
    // are now part of root_group. accumulated_group starts with root_group, so LGM(slot)
    // resolves each capability directly — no frame traversal or special fallback needed.
    let eval_ctx = eval_ctx.with_root_group_capabilities(capabilities);

    // Wrap the evaluation section in an async block so profiling cleanup runs unconditionally
    // even when loader setup fails.
    let eval_result: Result<(), String> = (async {
        // ── Evaluate loader.llt (or --init override) ─────────────────────────
        //
        // loader.llt is the tinct "main function". It reads %programs, %args, %cwd,
        // %libdir, %stdout from its initial scope, loads prelude, and runs the CLI pipeline.
        // All output happens via side effects (%stdout writes, emit channel drains).
        //
        // The initial environment (env) is the stdlib environment after all caps injection
        // above. %programs and %args are already injected into env before this point.
        //
        // run_loader_pipeline handles parse → desugar → resolve → typecheck →
        // eval → materialize. It is the shared bootstrap path used by both the CLI and
        // the lib API (eval_source_with_config).
        //
        // Dup the libdir into a plain cap_std::fs::Dir. libdir_rc_for_ctx is pre-opened
        // before Landlock fires; open_dir(".") produces a dup without a new ambient open.
        let libdir_for_loader = libdir_rc_for_ctx
            .as_ref()
            .ok_or_else(|| {
                "cannot evaluate loader.llt: stdlib directory (--libdir) is required".to_string()
            })?
            .open_dir(".")
            .map_err(|e| format!("cannot dup libdir for loader expansion: {e}"))?;

        // Determine init program source and path.
        // init_source_owned was pre-read before Landlock fired (above).
        let (init_source, init_path): (&str, &str) =
            if let (Some(ref path), Some(ref source)) = (&init, &init_source_owned) {
                (source.as_str(), path.as_str())
            } else {
                (include_str!("../stdlib/loader.llt"), "stdlib/loader.llt")
            };

        // Build injected type env: declares the types of every value injected into the
        // init program's scope by this CLI context. The type-checker for the init program
        // uses this so injected names like %programs, %cwd, %args are typed rather than
        // "undefined variable". Each injection site in main.rs is responsible for its type.
        let injected_type_env = {
            use tinct::{Env, Type};
            let dircap = || Type::TyCon("DirCap".to_string());
            let dict = || Type::TyCon("Dict".to_string());
            let netcap = || Type::TyCon("NetCap".to_string());
            let mut inj = Env::new();
            // CLI-injected values: types match what is actually injected above.
            inj.insert_injected("%programs".to_string(), dict());
            inj.insert_injected("%args".to_string(), dict());
            inj.insert_injected("%cwd".to_string(), dircap());
            inj.insert_injected("%libdir".to_string(), dircap());
            // User --cap-fs entries: each becomes a %NAME: DirCap.
            for (name, _cap_dir, _perms) in open_cap_fs_entries(&cap_fs, no_fs)? {
                let scoped = if name.starts_with('%') {
                    name.clone()
                } else {
                    format!("%{name}")
                };
                inj.insert_injected(scoped, dircap());
            }
            // User --cap-net entries: each becomes a %NAME: NetCap.
            for entry in &cap_net {
                if let Some((name, _)) = entry.split_once('=') {
                    let scoped = if name.starts_with('%') {
                        name.to_string()
                    } else {
                        format!("%{name}")
                    };
                    inj.insert_injected(scoped, netcap());
                }
            }
            Some(std::sync::Arc::new(std::sync::RwLock::new(inj)))
        };

        tinct::run_loader_pipeline(
            &eval_ctx,
            &libdir_for_loader,
            init_source,
            init_path,
            injected_type_env,
        )
        .await
    })
    .await; // end of eval_result async block

    // === Unconditional cleanup (runs on success AND failure) ===

    // Stop the background flush thread and perform a final LLT-stream drain.
    //
    // The flush thread is joined here in all exit paths (success, eval error, parse error,
    // type error, etc.) so the final spans are always written when --profile is set.
    // The thread exits by returning from its closure after seeing PROFILE_FLUSH_STOP=true.
    // We do NOT call _exit from the thread when stopped by the main thread — only the
    // SIGINT path calls _exit(130). Here the thread just returns normally.
    if let (Some(ref collector), Some(ref pfile), Some(handle)) =
        (&profiling_collector, &profile_file, flush_thread_handle)
    {
        // Signal the background thread to stop. Use SeqCst to ensure the thread sees
        // the flag before we join (prevents a race where the thread misses the flag
        // and loops back to sleep after we join).
        PROFILE_FLUSH_STOP.store(true, std::sync::atomic::Ordering::SeqCst);

        // Join the flush thread so its periodic writes complete before the final drain.
        // If the thread panicked, log a warning — we still want to write the remaining
        // spans below, so we do not propagate the error.
        if let Err(e) = handle.join() {
            eprintln!("tinct: warning: profiling background thread panicked: {e:?}");
        }

        // Final drain: write any spans the background thread has not yet seen.
        // The background thread may have written spans up to its last drain_new() call;
        // drain_new() here picks up only the remainder.
        let remaining = collector
            .lock()
            .expect("profiling: collector mutex poisoned — cannot flush remaining spans")
            .drain_new();

        if !remaining.is_empty() {
            use std::io::Write;
            match pfile.lock() {
                Ok(mut file_guard) => {
                    for span in &remaining {
                        let line = span.to_tinct_line();
                        if let Err(e) = writeln!(file_guard, "{}", line) {
                            eprintln!("profiling: final write error: {e}");
                            break;
                        }
                    }
                    if let Err(e) = file_guard.flush() {
                        eprintln!("profiling: final flush error: {e}");
                    }
                }
                Err(_) => {
                    eprintln!("profiling: final write skipped — file lock poisoned");
                }
            }
        } else {
            // No remaining spans; still flush to ensure background writes are committed.
            match pfile.lock() {
                Ok(mut file_guard) => {
                    use std::io::Write;
                    if let Err(e) = file_guard.flush() {
                        eprintln!("tinct: warning: failed to flush profile file: {e}");
                    }
                }
                Err(e) => {
                    eprintln!(
                        "tinct: profiling: collector mutex poisoned — skipping final flush: {e}"
                    );
                }
            }
        }
    }

    // Cancel any pending alarm.
    #[cfg(unix)]
    if timeout.is_some() {
        unsafe {
            libc::alarm(0);
        }
    }

    eval_result
}

// AMBIENT-OK: CLI bootstrap — opens file parent dir for type-checking
async fn run_fmt(
    file_path: &str,
    check: bool,
    in_place: bool,
    output_name: &str,
    strict: bool,
) -> Result<(), String> {
    let (sf_path, source) = read_source(file_path)?;

    // If --strict is set, typecheck the file first and fail if type errors exist.
    // Parse once and run the type checking pipeline on the parsed AST.
    // This avoids the double-parse that would happen if we called typecheck_source().
    if strict {
        let output = parse(&source, Arc::clone(&sf_path))
            .map_err(|e| tinct::format_parse_error(&e, &source, file_path))?;

        // PIPELINE INVARIANT: parse -> desugar -> resolve -> typecheck.
        let program = tinct::desugar::desugar_program_full(&output.program);
        let env_arc = tinct::get_builtin_core_type_env().await;
        let type_stage_scope = tinct::get_builtin_core_type_stage_scope().await;
        let (diagnostics, _env, _tycon_env) = tinct::typecheck::typecheck_program_bootstrap(
            &program,
            env_arc,
            None,
            std::collections::HashMap::new(),
            type_stage_scope,
        )
        .await;

        // Filter error-level diagnostics
        let type_errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| matches!(d.level, tinct::DiagnosticLevel::Err))
            .collect();

        if !type_errors.is_empty() {
            let error_msgs: Vec<String> = type_errors
                .iter()
                .map(|e| format_type_diagnostic(e, &source, file_path))
                .collect();
            return Err(error_msgs.join("\n"));
        }

        // Emit type quality diagnostics (T010/T011 Unknown, T012 overbroad, T013 ambiguous, …).
        // In --strict mode, bump each diagnostic's level and treat Err-level diagnostics
        // as fatal (they escalate Info→Warn→Err under --strict).
        {
            use tinct::DiagnosticLevel;
            let mut has_fatal_diag = false;
            for d in &diagnostics {
                let effective = if strict {
                    let bumped = d.bump_level();
                    if bumped.level == DiagnosticLevel::Err {
                        has_fatal_diag = true;
                    }
                    bumped
                } else {
                    d.clone()
                };
                eprintln!("{}", format_type_diagnostic(&effective, &source, file_path));
            }
            if strict && has_fatal_diag {
                return Err(
                    "type checking failed — type warnings escalated to errors by --strict"
                        .to_string(),
                );
            }
        }
    }

    // Validate formatter name: only alphanumeric and hyphens allowed.
    // This prevents path traversal via -o ../../secret or similar.
    if !output_name.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return Err(format!(
            "fmt: invalid formatter name {:?} (only alphanumeric and '-' allowed)",
            output_name
        ));
    }

    // Resolve and read the formatter script from %libdir/cli/fmt/<name>.llt.
    // AMBIENT-OK: CLI bootstrap — formatter script is loaded from the stdlib directory,
    // which is a trusted operator-controlled path resolved via find_libdir_path().
    let (script_source, script_name) = {
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
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("formatter.llt")
            .to_string();
        // AMBIENT-OK: CLI bootstrap — formatter script from operator-controlled stdlib path.
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read formatter script {}: {e}", path.display()))?;
        (content, name)
    };

    let use_compact = output_name == "compact";
    let formatted =
        tinct::format_source_tinct(&source, &script_source, &script_name, use_compact).await?;

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
        // AMBIENT-OK: CLI fmt --in-place writing to operator-specified file.
        std::fs::write(file_path, &formatted)
            .map_err(|e| format!("error writing {file_path}: {e}"))?;
        return Ok(());
    }

    print!("{formatted}");
    Ok(())
}

// format_type_diagnostic is now pub in tinct::format_type_diagnostic (src/lib.rs).

/// Compute the blake3 hash of a file and print `blake3:<hexdigest>`.
/// Used to generate integrity hashes for `$include` second arguments.
// AMBIENT-OK: CLI hash command on operator-specified file.
fn run_hash(file_path: &str) -> Result<(), String> {
    let mut buf = Vec::new();
    std::fs::File::open(file_path)
        .map_err(|e| format!("error reading file: {e}"))?
        .read_to_end(&mut buf)
        .map_err(|e| format!("error reading file: {e}"))?;
    let hash = blake3::hash(&buf);
    println!("blake3:{}", hash.to_hex());
    Ok(())
}

/// Read LLT source from a file path or stdin (when path is `-`).
///
/// Returns a `(Arc<str>, String)` pair of (file_path, source_content) ready to be
/// threaded into `parse()` so that all spans in the parsed AST carry the file path.
// AMBIENT-OK: CLI entry point reading operator-specified file.
fn read_source(file_path: &str) -> Result<(Arc<str>, String), String> {
    if file_path == "-" {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("error reading stdin: {e}"))?;
        Ok((Arc::from("-"), buf))
    } else {
        let mut buf = String::new();
        std::fs::File::open(file_path)
            .map_err(|e| format!("error reading file: {e}"))?
            .read_to_string(&mut buf)
            .map_err(|e| format!("error reading file: {e}"))?;
        Ok((Arc::from(file_path), buf))
    }
}

/// Configuration for literate mode operations.
struct LiterateConfig<'a> {
    file_path: &'a str,
    mode: &'a LiterateMode,
    strict: bool,
    in_place: bool,
    verify: bool,
    fail_on_errors: bool,
    cap_fs: &'a [String],
    cap_net: &'a [String],
}

/// Process a Markdown file in literate mode.
///
/// Extracts ```` ```tinct ```` and ```` ```llt ```` fenced code blocks and
/// handles them according to `mode`:
///
/// - **`tangle`** — print the extracted blocks joined with `\n---\n`.
// AMBIENT-OK: CLI bootstrap — opens file parent dir for evaluation
async fn run_literate(config: &LiterateConfig<'_>) -> Result<(), String> {
    let file_path = config.file_path;
    let (_, markdown) = read_source(file_path)?;
    let blocks = literate::extract_code_blocks(&markdown);

    if blocks.is_empty() {
        match config.mode {
            LiterateMode::Tangle | LiterateMode::Eval => {
                // Nothing to print — output empty string (no trailing newline).
                return Ok(());
            }
            LiterateMode::Weave => {
                // No blocks to weave — print markdown unchanged.
                print!("{markdown}");
                return Ok(());
            }
        }
    }

    match config.mode {
        LiterateMode::Tangle => {
            let tangled = literate::tangle(blocks);
            println!("{tangled}");
            Ok(())
        }

        LiterateMode::Eval => {
            let tangled = literate::tangle(blocks);
            run_literate_eval(&tangled, config).await
        }

        LiterateMode::Weave => run_literate_weave(&markdown, &blocks, config).await,
    }
}

/// Eval mode: tangle literate blocks and evaluate as a pipeline.
///
/// Extracts code blocks, joins them into a pipeline source, and evaluates
/// the result through the full tinct pipeline (parse → eval → output).
/// The output is the JSON representation of the final pipeline value.
///
/// AMBIENT-OK: CLI literate-eval opening markdown file directory for evaluation.
async fn run_literate_eval(tangled: &str, config: &LiterateConfig<'_>) -> Result<(), String> {
    let markdown_path = config.file_path;

    // Evaluate via run_eval with the tangled source as a single -e expression.
    // This reuses the full CLI pipeline (loader, prelude, formatters) identically to
    // `tinct run -e "$tangled"` invoked from the markdown file's directory.
    run_eval(RunArgs {
        files: vec![],
        no_fs: false,
        require_integrity: false,
        strict: config.strict,
        timeout: None,
        no_landlock: true, // literate mode does not sandbox
        no_env: false,
        allow_env: vec![],
        no_cwd: false,
        no_libdir: false,
        libdir_path: None,
        cap_fs: config.cap_fs.to_vec(),
        cap_net: config.cap_net.to_vec(),
        no_cap_clock: false,
        cap_clock_fixed: None,
        cap_file: vec![],
        init: None,
        expr: vec![tangled.to_string()],
        input: None,
        output: None,
        profile: None,
    })
    .await
    .map_err(|e| {
        // Prefix errors with the markdown file path for context.
        format!("{}: {}", markdown_path, e)
    })?;

    // run_eval handles output formatting and writing to stdout.
    Ok(())
}

/// Weave mode: evaluate literate blocks and annotate the markdown with results.
///
/// Evaluates each code block's cumulative prefix (all blocks up to and including
/// the current one) as a pipeline. The output of each cumulative evaluation is
/// inserted as an `=== out` section after the corresponding code block.
///
/// Uses the same evaluation pipeline as `tinct run -e` for correctness.
/// Each cumulative prefix is evaluated independently (no shared state between
/// block evaluations), matching the pipeline semantics where % threads between
/// documents.
///
/// AMBIENT-OK: CLI literate-weave opening markdown file directory for evaluation.
async fn run_literate_weave(
    markdown: &str,
    blocks: &[String],
    config: &LiterateConfig<'_>,
) -> Result<(), String> {
    // Extract blocks with byte offset information for reinsertion.
    let lit_blocks = literate::extract_blocks(markdown);

    if lit_blocks.is_empty() {
        // No blocks to weave — print markdown unchanged.
        print!("{markdown}");
        return Ok(());
    }

    // Evaluate each block's cumulative prefix and collect outputs.
    // Each prefix is evaluated via run_eval with stdout redirected to a string.
    // Since run_eval writes to stdout directly and we can't easily capture that,
    // we use the simpler approach: evaluate the full pipeline once and annotate
    // the last block with the result.
    //
    // For per-block outputs, each cumulative prefix is evaluated as a separate
    // tinct invocation. Output is captured by redirecting through a child process.
    let mut block_outputs: Vec<Option<String>> = Vec::with_capacity(lit_blocks.len());

    for i in 0..blocks.len() {
        // Build cumulative source: blocks[0..=i] joined with ---
        let cumulative_blocks: Vec<String> = blocks[..=i].to_vec();
        let cumulative_source = literate::tangle(cumulative_blocks);

        // Evaluate via a child tinct process to capture stdout.
        // Find the tinct binary path (current executable).
        let tinct_exe = std::env::current_exe()
            .map_err(|e| format!("cannot determine tinct executable path: {e}"))?;

        let child_result = tokio::process::Command::new(&tinct_exe)
            .arg("run")
            .arg("-e")
            .arg(&cumulative_source)
            .output()
            .await
            .map_err(|e| format!("cannot spawn tinct for block {}: {e}", i + 1))?;

        if child_result.status.success() {
            let stdout = String::from_utf8_lossy(&child_result.stdout);
            let output = stdout.trim().to_string();
            block_outputs.push(if output.is_empty() {
                None
            } else {
                Some(output)
            });
        } else {
            let stderr = String::from_utf8_lossy(&child_result.stderr);
            let error_msg = stderr.trim().to_string();
            if config.fail_on_errors {
                return Err(format!("error in block {}: {}", i + 1, error_msg));
            }
            block_outputs.push(Some(format!("ERROR: {error_msg}")));
        }
    }

    // Reconstruct the markdown with === out sections updated.
    // Process blocks from last to first so byte offsets remain valid.
    let mut result = markdown.to_string();
    for (i, lit_block) in lit_blocks.iter().enumerate().rev() {
        if let Some(ref output_text) = block_outputs.get(i).and_then(|o| o.as_ref()) {
            // Find the closing ``` fence position.
            let fence_end = lit_block.md_code_end;
            // Find the end of the closing ``` line.
            let closing_line_end = result[fence_end..]
                .find('\n')
                .map(|p| fence_end + p + 1)
                .unwrap_or(result.len());

            // Build the === out section.
            let new_section = format!("=== out\n{output_text}\n");

            // Check if there's already an === out section (inside the code block).
            // The extract_blocks function already splits code from expectations.
            // We insert === out AFTER the closing fence, before the next prose.
            result.insert_str(closing_line_end, &new_section);
        }
    }

    if config.in_place {
        // Write back to the source file atomically.
        let tmp_path = format!("{}.tmp", config.file_path);
        std::fs::write(&tmp_path, &result)
            .map_err(|e| format!("cannot write temp file {tmp_path}: {e}"))?;
        std::fs::rename(&tmp_path, config.file_path)
            .map_err(|e| format!("cannot rename {tmp_path} → {}: {e}", config.file_path))?;
    } else {
        print!("{result}");
    }

    if config.verify {
        // Compare actual outputs against expected === sections.
        for (i, lit_block) in lit_blocks.iter().enumerate() {
            if let Some(ref expected) = lit_block.expectations.out {
                if let Some(Some(ref actual)) = block_outputs.get(i) {
                    if actual.trim() != expected.trim() {
                        return Err(format!(
                            "block {} output mismatch:\n  expected: {}\n  actual:   {}",
                            i + 1,
                            expected.trim(),
                            actual.trim()
                        ));
                    }
                }
            }
        }
    }

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

        "E043" => {
            "\
E043: Resource limit exceeded

An operation exceeded a configured resource limit (such as collection
size or string length).

Fix: reduce the size of the collection or string, or check whether the
limit can be raised for your use case."
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

        "E060" => {
            "\
E060: Parse conversion failed

A $to-int or $to-float call could not parse the supplied string.

Fix: ensure the string is a valid integer or floating-point literal before
converting, or use $try to handle the error gracefully."
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

        "T017" => {
            "\
T017: Instance pattern contains Unknown types (type checker)

An instance match arm (for a typeclass instance declaration) contains one or
more pattern positions whose types are Unknown. The type checker requires all
pattern positions to have concrete type annotations — Unknown prevents
coherence checking from working correctly.

Example of the error:
  instance Eq [MyClass a b]:  -- 'a' or 'b' lacks a type annotation

Fix:
  - Add explicit type annotations to all pattern positions using the a@Type
    syntax, e.g. [MyClass a@Int b@String].
  - Ensure all type parameters in the instance head have concrete types that
    the type checker can resolve."
        }

        "match-pattern-mismatch" => {
            "\
match-pattern-mismatch: Undefined constructor in structural test (type checker)

A [case [let v: ConstructorName] body] arm references a constructor name that
is not defined in any enclosing scope. The arm will never match at runtime
because the constructor tag cannot be produced if the constructor is unknown.

Common causes:
  - Typo in the constructor name (e.g., `Ok` vs `Ok_`).
  - The type whose constructors are being matched is not in scope.
  - The constructor belongs to a different variant type than the scrutinee.

Fix: check the spelling of the constructor name, ensure the type is imported,
and verify the scrutinee's type has a constructor with that name."
        }

        "match-guard-failure" => {
            "\
match-guard-failure: Nullary constructor used as payload-binding structural test (type checker)

A [case [let v: ConstructorName] body] arm attempts to bind v to the payload
of a nullary constructor (one that carries no value). Since a nullary constructor
has no payload, v can never be bound, and this arm is structurally dead.

Fix: use [let _: ConstructorName] to match the constructor tag without binding,
or use a constructor that carries a payload if you need to extract a value."
        }

        "match-exhaustiveness" => {
            "\
match-exhaustiveness: Dead match arm — pattern type disjoint from scrutinee type (type checker)

A match arm has a pattern type that is provably disjoint from the scrutinee type,
meaning the arm can never match at runtime. The type checker has determined that
no value can inhabit both the scrutinee type and the pattern type simultaneously.

Common examples:
  - Pattern expects Int, but scrutinee is Str
  - Pattern expects a specific constructor tag that the scrutinee type cannot produce

This is a WARNING, not a hard error — the arm is still evaluated if somehow reached
at runtime (e.g., if the type checker's knowledge was incomplete). However, under
normal circumstances, this arm is unreachable.

Fix: remove the dead arm, or if the types are incorrect, update the pattern type
annotation or the scrutinee expression to align with the intended logic."
        }

        "unknown-type-param" => {
            "\
unknown-type-param: Unknown type parameter annotation (type checker)

A type alias declaration uses an `@X` annotation on a type parameter where X is
neither a recognized variance keyword (Covariant, Contravariant, Invariant, Phantom)
nor a registered type class name.

Fix: use a valid variance annotation or a registered class name (e.g., @Covariant, @Equatable)."
        }

        _ => {
            return Err(format!(
                "unknown error code: {code}\n\
                 Run 'tinct explain <code>' with a valid code, e.g. E001 through E099 or T000-T004.\n\
                 Known codes: E001, E002, E010, E011, E020-E024, E030-E036, \
                 E043, E051-E056, E060, E063, E070, E080, E090, E099, \
                 T000, T001, T002, T003, T004, T017, \
                 match-pattern-mismatch, match-guard-failure, match-exhaustiveness, unknown-type-param."
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
        assert_eq!(parse_duration("1m"), Ok(60));
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
