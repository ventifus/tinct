//! LLT command-line tool: parses and evaluates `.llt` files, outputs JSON or LLT display format.

#![deny(clippy::disallowed_types, clippy::disallowed_methods)]
// Arc<Thunk> and related types are !Send because Thunk contains Rc<...>. LLT uses
// tokio::task::LocalSet with a current_thread runtime — values never cross threads.
#![allow(clippy::arc_with_non_send_sync)]

use clap::{Parser, Subcommand, ValueEnum};
use std::io::{self, Read};
use std::path::PathBuf;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use tinct::{
    build_core_env, literate, parse, parse_with_file, string_val, EvalContext, HashableValue,
    SourceFile, Thunk, ThunkId, Value, MAX_FILE_SIZE,
};
// Exit codes for llt eval
const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_TIMEOUT: i32 = 2;
// tinct::limit_alloc::EXIT_OOM (3): soft heap-limit exceeded — diagnostics printed, clean exit.
// Note: RLIMIT_AS violations (hard backstop) cause abort via handle_alloc_error, not a clean exit.
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

#[allow(clippy::large_enum_variant)] // Run variant contains all CLI flags
#[derive(Subcommand)]
enum Commands {
    /// Evaluate an LLT file and output the result.
    #[clap(alias = "eval")]
    Run {
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
    /// Describe the input contract of an LLT file.
    ///
    /// Extracts `%@Type` annotations and schema dicts, printing a human-readable
    /// summary of the expected input shape.
    Describe {
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
    /// Runs parse → desugar → typecheck pipeline.
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
        /// Format: NAME=PATH:MODE — binds %NAME to a DirCap. MODE is required.
        /// MODE is one or more of: r (read), w (write), l (list), s (stat).
        /// Example: docs=/tmp/mydocs:rl injects %docs with read+list access.
        #[arg(long, value_name = "NAME=PATH:MODE")]
        cap_fs: Vec<String>,

        /// Inject a named NetCap into the root environment (may be repeated).
        /// Format: NAME=ENTRY — binds %NAME to a NetCap.
        /// Multiple uses of the same NAME accumulate into one NetCap allowlist.
        #[arg(long, value_name = "NAME=ENTRY")]
        cap_net: Vec<String>,

        /// Type errors are fatal (exit 1). Without --strict, type warnings are advisory.
        #[arg(long)]
        strict: bool,
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
    /// Extract blocks, type-check without evaluating.
    Lint,
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
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();

    // Soft heap limit: fires before RLIMIT_AS, prints diagnostics, exits cleanly.
    // Works on all platforms; no-op when --max-memory is not passed.
    if let Some(max_bytes) = cli.max_memory {
        if max_bytes > 0 {
            tinct::limit_alloc::set_limit(max_bytes);
        }
    }

    // Hard RLIMIT_AS backstop: catches anything that bypasses the allocator
    // (direct mmap, stack growth, shared-library mappings).
    #[cfg(unix)]
    if let Err(e) = setup_rlimits(cli.max_memory, cli.max_cpu, cli.max_fds) {
        eprintln!("error: {e}");
        return EXIT_ERROR;
    }
    // On non-Unix platforms rlimit flags are accepted for CLI compatibility but have no effect.
    #[cfg(not(unix))]
    {
        let _ = cli.max_memory;
        let _ = cli.max_cpu;
        let _ = cli.max_fds;
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
            let _ = libc::setrlimit(libc::RLIMIT_STACK, &new_rl);
        }
    }

    // Materialize is iterative (materialize_rc loop); no large worker stack needed.
    let result = match cli.command {
        Commands::Run {
            no_fs,
            require_integrity,
            strict,
            timeout,
            no_landlock,
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
            profile,
            files,
        } => {
            run_eval(
                &files,
                no_fs,
                require_integrity,
                strict,
                timeout.as_deref(),
                no_landlock,
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
                profile.as_deref(),
            )
            .await
        }
        Commands::Hash { file } => run_hash(&file),
        Commands::Fmt {
            check,
            in_place,
            output,
            strict,
            file,
        } => run_fmt(&file, check, in_place, &output, strict).await,
        Commands::Describe { file } => run_describe(&file).await,
        Commands::Explain { code } => run_explain(&code),
        Commands::Lint {
            file,
            no_fs,
            cap_fs,
            cap_net,
            strict,
        } => run_lint(&file, no_fs, strict, &cap_fs, &cap_net).await,
        Commands::Literate {
            mode,
            file,
            no_substitute: _,
            strict,
            in_place: _,
            verify: _,
            fail_on_errors: _,
            cap_fs: _,
            cap_net: _,
        } => {
            run_literate(&LiterateConfig {
                file_path: &file,
                mode: &mode,
                strict,
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

/// Open a base directory for the given file path, using the file's parent directory.
/// Falls back to "." if the path has no parent or is "-" (stdin).
///
/// AMBIENT-OK: Helper for CLI bootstrap — operator specified file paths.
#[allow(clippy::disallowed_methods)]
fn open_file_base_dir(file_path: &str, context: &str) -> Result<cap_std::fs::Dir, String> {
    let dir_path = if file_path == "-" {
        std::path::Path::new(".")
    } else {
        let p = std::path::Path::new(file_path);
        p.parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."))
    };
    cap_std::fs::Dir::open_ambient_dir(dir_path, cap_std::ambient_authority())
        .map_err(|e| format!("{context}: cannot open base directory: {e}"))
}

/// Open cap_std::fs::Dir entries for the given --cap-fs list.
/// Skips injection when no_fs is true.
/// Returns Vec<(name, Arc<cap_std::fs::Dir>, perms)>.
// AMBIENT-OK: CLI bootstrap — operator-specified --cap-fs paths
#[allow(clippy::disallowed_methods)]
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
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
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
                            eprintln!("profiling: background flush file lock poisoned");
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

#[allow(clippy::too_many_arguments)]
// CLI entrypoint with all flags
// AMBIENT-OK: CLI bootstrap — operator-specified file paths and capability directories
#[allow(clippy::disallowed_methods)]
async fn run_eval(
    file_paths: &[String],
    no_fs: bool,
    require_integrity: bool,
    strict: bool,
    timeout: Option<&str>,
    no_landlock: bool,
    no_env: bool,
    allow_env: Vec<String>,
    no_cwd: bool,
    no_libdir: bool,
    libdir_path: Option<String>,
    cap_fs: Vec<String>,
    cap_net: Vec<String>,
    no_cap_clock: bool,
    cap_clock_fixed: Option<String>,
    cap_file: Vec<String>,
    init: Option<String>,
    expr: Vec<String>,
    input: Option<String>,
    output: Option<String>,
    profile: Option<&str>,
) -> Result<(), String> {
    // Build the interleaved list of user pipeline stages: files and -e expressions in CLI order.
    // Clap doesn't preserve mixed positional/flag order, so we reconstruct it by parsing raw args.
    //
    // NOTE (T-1347): The -i/-o formatter stages are NO LONGER added here Rust-side.
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
    struct PreOpenedFile {
        abs_path: String,
        /// Readable bytes handle opened pre-Landlock.
        // AMBIENT-OK: CLI bootstrap — operator-specified file paths.
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
            // AMBIENT-OK: CLI bootstrap — operator-specified file path.
            #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
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
    let init_source_owned: Option<String> = if let Some(ref path) = init {
        #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
        let source = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read --init file '{}': {e}", path))?;
        Some(source)
    } else {
        None
    };

    // Install timeout handler if requested (must happen before evaluation)
    if let Some(duration) = timeout {
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

    // T-1557: Env is type-metadata only; runtime thunks go into the FlatEnv arena.
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
        let mut extra_readable: Vec<PathBuf> = pre_opened_files
            .iter()
            .filter_map(|(_, pf)| {
                let path = std::path::Path::new(&pf.abs_path);
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
            dir: Rc::new(cwd_dir),
            perms: tinct::DirPerms::full(),
        };
        let cwd_thunk = Arc::new(tinct::Thunk::new_materialized(
            cwd_value,
            tinct::rust_span!(),
        ));
        env.write()
            .unwrap()
            .insert_slot_name_only("%cwd".to_string());
        deferred_cap_thunks.push(("%cwd".to_string(), cwd_thunk));
    }

    // Inject %arena as Value::Arena { name: "root", start_env_id: 0 } so that tinct
    // programs can access the root evaluation arena as a named capability.
    // start_env_id: 0 is the root FlatEnv allocated by EvalContext at construction.
    // Previously deferred; now implemented.
    {
        let arena_value = Value::Arena {
            name: "root".into(),
            start_env_id: 0,
        };
        let arena_thunk = Arc::new(tinct::Thunk::new_materialized(
            arena_value,
            tinct::rust_span!(),
        ));
        env.write()
            .unwrap()
            .insert_slot_name_only("%arena".to_string());
        deferred_cap_thunks.push(("%arena".to_string(), arena_thunk));
    }

    // %stdin: Handle/WriteHandle removed. When -i is specified, %stdin is not injected.
    // The input formatter (cli/in/*.llt) must be updated to use builtin-read-stdin instead.
    // TODO: Redesign %stdin injection using Value::File or builtin-read-stdin for each read.
    // %stdin injection via Handle removed — network/stream redesign in progress.
    // The -i flag sets input formatter; %stdin is not injected. Input format name flows via %args.input.
    if input.is_some() {
        // No-op: %stdin was previously injected here as a Value::Handle. Now no-op.
        // The input formatter name is passed via %args.input (see below).
    }

    // NOTE: %stdout and %stderr are NOT injected here.
    // They are defined as protocol dicts in loader.llt Dict 2, using
    // builtin-write-stdout and builtin-write-stderr (stateless builtins).

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
                // Clone the Dir for the DirCap value (needs Rc<Dir>)
                let libdir_dir_for_cap =
                    Rc::new(libdir_arc.open_dir(".").expect("failed to dup libdir"));
                let libdir_value = Value::DirCap {
                    dir: libdir_dir_for_cap,
                    perms: tinct::DirPerms::full(),
                };
                let libdir_thunk = Arc::new(tinct::Thunk::new_materialized(
                    libdir_value,
                    tinct::rust_span!(),
                ));
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
            // Clone the Arc to get an independent Rc for the DirCap value
            let dir_for_cap = Rc::new(cap_dir_arc.open_dir(".").expect("failed to dup cap dir"));
            let cap_value = Value::DirCap {
                dir: dir_for_cap,
                perms,
            };
            let cap_thunk = Arc::new(tinct::Thunk::new_materialized(
                cap_value,
                tinct::rust_span!(),
            ));
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
            let cap_value = Value::NetCap(Rc::new(entries));
            let cap_thunk = Arc::new(tinct::Thunk::new_materialized(
                cap_value,
                tinct::rust_span!(),
            ));
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
            Value::ClockCap(Rc::new(ClockCapInner::Fixed(nanos)))
        } else {
            // Default: real system clock
            Value::ClockCap(Rc::new(ClockCapInner::Real))
        };

        let cap_thunk = Arc::new(tinct::Thunk::new_materialized(
            cap_value,
            tinct::rust_span!(),
        ));
        env.write()
            .unwrap()
            .insert_slot_name_only("%clock".to_string());
        deferred_cap_thunks.push(("%clock".to_string(), cap_thunk));
    }

    // Inject --cap-file NAME=PATH[:MODE] entries into the root environment as `%NAME`.
    // --no-fs suppresses all cap-file entries (filesystem access is blocked globally).
    // AMBIENT-OK: CLI bootstrap — operator-specified file paths via --cap-file.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    if !no_fs {
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

            // --cap-file: Handle/WriteHandle removed. Use DirCap (--cap-fs) instead.
            // TODO: Redesign --cap-file to inject a DirCap or Value::File.
            return Err(format!(
                "--cap-file: file handle injection via Handle/WriteHandle is not available in this version. \
                 Use --cap-fs NAME=PATH to inject a DirCap instead. Affected entry: {:?}",
                cap_file_entry
            ));
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
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
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

    // ── Build %programs Dict (T-1347) ─────────────────────────────────────────
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
    let cwd_for_ctx = {
        // AMBIENT-OK: CWD at startup, operator-controlled.
        #[allow(clippy::disallowed_methods)]
        cap_std::fs::Dir::open_ambient_dir(
            std::env::current_dir()
                .map_err(|e| format!("cannot determine working directory: {e}"))?,
            cap_std::ambient_authority(),
        )
        .map_err(|e| format!("cannot open cwd for eval context: {e}"))?
    };
    let eval_ctx = {
        let mut ctx = EvalContext::new_with_options(
            cwd_for_ctx,
            no_fs,
            require_integrity,
            env_allowed.clone(),
        );
        if let Some(ref collector) = profiling_collector {
            Arc::get_mut(&mut ctx).unwrap().profiling = Some(Arc::clone(collector));
        }
        if let Some(ref libdir_rc) = libdir_rc_for_ctx {
            ctx.set_libdir_dir(Arc::clone(libdir_rc));
        }
        // Initialize TypeContext so loader.llt can call [builtin-get-type-context].
        // tycon_env starts empty — it is populated by uses-scope calling builtin-typecheck
        // for each module in the --- uses: header (core first, then io, etc.).
        ctx.init_type_context(tinct::TypeContextData {
            type_stage_env: std::sync::Arc::new(std::sync::RwLock::new(tinct::Env::new())),
            type_stage_flat_env_id: None,
            inference_env: tinct::get_builtin_core_type_env()
                .await
                .expect("builtin_core type env unavailable at startup"),
            tycon_env: std::collections::HashMap::new(),
        });
        ctx
    };

    // T-1577: Inject deferred cap thunks as NAMED bindings into the root FlatEnv.
    // The resolver (T-1576) seeds from FlatEnv.slot_names, so each capability must be
    // allocated with its name ("%libdir", "%cwd", etc.) so the resolver assigns de Bruijn
    // coordinates for it. Ordering MUST match registration order above.
    for (name, thunk) in deferred_cap_thunks {
        eval_ctx.alloc_named_thunk(&name, thunk);
    }

    // Helper: allocate a value as a materialized thunk in the eval_ctx arena and return its ThunkId.
    // Used to build Value::Dict entries (which use ThunkId, not Arc<Thunk>).
    let alloc_val = |v: Value| -> ThunkId {
        eval_ctx.alloc_thunk(Arc::new(Thunk::new_materialized(v, tinct::rust_span!())))
    };

    // Build %programs as an integer-keyed Value::Dict.
    // Each entry is a Value::Variant (ProgramItem.File or ProgramItem.Expr) ThunkId.
    //
    // Value::Dict uses ThunkId keys (arena-based), not Arc<Thunk>.
    // All thunks allocated here use the eval_ctx arena created above.
    let programs_dict: Value = {
        use indexmap::IndexMap;
        let mut pre_open_iter = pre_opened_files.into_iter();
        let mut dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();

        for (index, stage) in interleaved_stages.iter().enumerate() {
            let key = HashableValue::Int(index as i64);
            let item_value = match stage {
                PipelineStage::File(file_path) => {
                    if file_path == "-" {
                        // stdin: ProgramItem.File with path="-", no handle.
                        // loader.llt eval-file checks path=="-" and reads from %stdin.
                        let mut payload_dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();
                        payload_dict.insert(
                            HashableValue::Str("path".into()),
                            alloc_val(string_val("-")),
                        );
                        let payload_id = alloc_val(Value::Dict(payload_dict));
                        Value::Variant {
                            tag: "ProgramItem.File".to_string(),
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
                        use std::cell::RefCell;
                        let cap_file = cap_std::fs::File::from_std(raw_handle);
                        let handle_value = Value::File(Rc::new(RefCell::new(cap_file)));

                        // Build payload dict: { path: String, handle: Handle }
                        let mut payload_dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();
                        payload_dict.insert(
                            HashableValue::Str("path".into()),
                            alloc_val(string_val(&abs_path)),
                        );
                        payload_dict
                            .insert(HashableValue::Str("handle".into()), alloc_val(handle_value));
                        let payload_id = alloc_val(Value::Dict(payload_dict));
                        Value::Variant {
                            tag: "ProgramItem.File".to_string(),
                            payload: Some(payload_id),
                        }
                    }
                }
                PipelineStage::Expr(expression) => {
                    // ProgramItem.Expr { src: String }
                    let mut payload_dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();
                    payload_dict.insert(
                        HashableValue::Str("src".into()),
                        alloc_val(string_val(expression)),
                    );
                    let payload_id = alloc_val(Value::Dict(payload_dict));
                    Value::Variant {
                        tag: "ProgramItem.Expr".to_string(),
                        payload: Some(payload_id),
                    }
                }
            };
            dict.insert(key, alloc_val(item_value));
        }
        Value::Dict(dict)
    };

    // ── Build %args Dict (T-1347) ──────────────────────────────────────────────
    //
    // Dict with parsed CLI flags. loader.llt reads %args.output to select the formatter
    // and %args.strict to decide whether type errors are fatal.
    //
    // %args.input: name of the -i input formatter (or "" if not specified).
    // The -i formatter is NOT pre-appended to %programs Rust-side. Instead, loader.llt
    // dict 3 reads %args.input to construct the input ProgramItem.File if non-empty.
    let args_dict: Value = {
        use indexmap::IndexMap;
        let mut dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();

        // output: name of the -o formatter (default "none").
        dict.insert(
            HashableValue::Str("output".into()),
            alloc_val(string_val(output.as_deref().unwrap_or("none"))),
        );

        // input: name of the -i formatter (default "" = no input formatter).
        dict.insert(
            HashableValue::Str("input".into()),
            alloc_val(string_val(input.as_deref().unwrap_or(""))),
        );

        // strict: whether type errors are fatal.
        dict.insert(
            HashableValue::Str("strict".into()),
            alloc_val(Value::Int(if strict { 1 } else { 0 })),
        );

        Value::Dict(dict)
    };

    // Inject %programs and %args into the stdlib environment so loader.llt can see them.
    // T-1557: Register slot names in env (for resolver) and thunks in arena (for evaluator).
    {
        let programs_thunk = std::sync::Arc::new(tinct::Thunk::new_materialized(
            programs_dict,
            tinct::rust_span!(),
        ));
        eval_ctx.alloc_named_thunk("%programs", programs_thunk);
        let args_thunk = std::sync::Arc::new(tinct::Thunk::new_materialized(
            args_dict,
            tinct::rust_span!(),
        ));
        eval_ctx.alloc_named_thunk("%args", args_thunk);
    }

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

        tinct::run_loader_pipeline(&eval_ctx, &libdir_for_loader, no_fs, init_source, init_path)
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
        // If the thread panicked (shouldn't happen), ignore the join error — we still
        // want to write the remaining spans below.
        let _ = handle.join();

        // Final drain: write any spans the background thread has not yet seen.
        // The background thread may have written spans up to its last drain_new() call;
        // drain_new() here picks up only the remainder.
        let remaining = match collector.lock() {
            Ok(mut guard) => guard.drain_new(),
            Err(_) => vec![],
        };

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
            if let Ok(mut file_guard) = pfile.lock() {
                use std::io::Write;
                let _ = file_guard.flush();
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
#[allow(clippy::disallowed_methods)]
async fn run_fmt(
    file_path: &str,
    check: bool,
    in_place: bool,
    output_name: &str,
    strict: bool,
) -> Result<(), String> {
    let sf = read_source(file_path)?;
    let source = String::from(&*sf.content);

    // If --strict is set, typecheck the file first and fail if type errors exist.
    // Parse once and run the type checking pipeline on the parsed AST.
    // This avoids the double-parse that would happen if we called typecheck_source().
    if strict {
        let output = parse_with_file(&source, Arc::clone(&sf))
            .map_err(|e| tinct::format_parse_error(&e, &source, file_path))?;

        // PIPELINE INVARIANT: parse -> desugar -> resolve -> typecheck.
        let mut program = output.program;
        tinct::desugar::desugar_surface_program(&mut program);
        let env_arc = tinct::get_builtin_core_type_env()
            .await
            .expect("builtin core type env unavailable");
        let (type_errors, _type_map, _doc_map, _scheme_map, fmt_diagnostics) =
            tinct::typecheck::typecheck_surface_program(&program, env_arc).await;

        if !type_errors.is_empty() {
            let error_msgs: Vec<String> = type_errors
                .iter()
                .map(|e| tinct::format_type_error(e, &source, file_path))
                .collect();
            return Err(error_msgs.join("\n"));
        }

        // Emit type quality diagnostics (T010/T011 Unknown, T012 overbroad, T013 ambiguous, …).
        // In --strict mode, bump each diagnostic's level and treat Err-level diagnostics
        // as fatal (they escalate Info→Warn→Err under --strict).
        {
            use tinct::DiagnosticLevel;
            let mut has_fatal_diag = false;
            for d in &fmt_diagnostics {
                let effective = if strict {
                    let bumped_level = d.level.bump();
                    let bumped = tinct::TypeDiagnostic {
                        level: bumped_level,
                        message: d.message.clone(),
                        span: d.span.clone(),
                        code: d.code,
                    };
                    if bumped_level == DiagnosticLevel::Err {
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
    // Pass the file's directory as an already-open Dir to avoid re-acquiring ambient authority.
    let fmt_base_dir_for_formatter = open_file_base_dir(file_path, "fmt").ok();
    let formatted =
        tinct::format_source_tinct_with_dir(&source, &script_path, fmt_base_dir_for_formatter)
            .await?;

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
        #[allow(clippy::disallowed_methods)]
        std::fs::write(file_path, &formatted)
            .map_err(|e| format!("error writing {file_path}: {e}"))?;
        return Ok(());
    }

    print!("{formatted}");
    Ok(())
}

/// Type-check a file without evaluating.
/// Exit 0 on clean, exit 1 on any warnings or errors.
// AMBIENT-OK: CLI bootstrap — opens file parent dir for type-checking
#[allow(clippy::disallowed_methods)]
async fn run_lint(
    file_path: &str,
    _no_fs: bool,
    strict: bool,
    // cap_fs is accepted by the CLI for consistency but not injected into the
    // lint environment. Lint does not evaluate code, so DirCap injection is
    // unnecessary. --no-fs is the default for lint.
    _cap_fs: &[String],
    _cap_net: &[String],
) -> Result<(), String> {
    let sf = read_source(file_path)?;
    let source = String::from(&*sf.content);

    // Parse the file
    let output = parse_with_file(&source, Arc::clone(&sf))
        .map_err(|e| tinct::format_parse_error(&e, &source, file_path))?;

    // PIPELINE INVARIANT: parse -> desugar -> resolve -> typecheck.
    let mut program = output.program;
    tinct::desugar::desugar_surface_program(&mut program);
    let env_arc = tinct::get_builtin_core_type_env()
        .await
        .expect("builtin core type env unavailable");
    let (type_errors, _type_map, _doc_map, _scheme_map, diagnostics) =
        tinct::typecheck::typecheck_surface_program(&program, env_arc).await;

    // Collect all errors and warnings
    let mut all_messages = Vec::new();

    for e in &type_errors {
        all_messages.push(tinct::format_type_error(e, &source, file_path));
    }

    // In --strict mode, bump each diagnostic's level before display (Info→Warn, Warn→Err),
    // then treat Err-level diagnostics as fatal. Mirrors run_eval and run_fmt behavior.
    let mut has_fatal_diag = false;
    for d in &diagnostics {
        let effective = if strict {
            use tinct::DiagnosticLevel;
            let bumped_level = d.level.bump();
            if bumped_level == DiagnosticLevel::Err {
                has_fatal_diag = true;
            }
            tinct::TypeDiagnostic {
                level: bumped_level,
                message: d.message.clone(),
                span: d.span.clone(),
                code: d.code,
            }
        } else {
            d.clone()
        };
        all_messages.push(format_type_diagnostic(&effective, &source, file_path));
    }

    // Type errors always fatal; diagnostics (warnings) fatal only with --strict (at Err level)
    let fatal_count = if strict {
        type_errors.len() + if has_fatal_diag { 1 } else { 0 }
    } else {
        type_errors.len()
    };

    if !all_messages.is_empty() {
        eprintln!("{}", all_messages.join("\n"));
    }

    if fatal_count > 0 {
        return Err(format!("lint failed with {} issue(s)", fatal_count));
    }

    // Clean — exit 0 (no output on success)
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
    if let Some(snippet) = tinct::render_span_snippet(source, diag.span.clone()) {
        out.push_str("  |\n");
        out.push_str(&snippet);
    }

    out
}

/// Compute the blake3 hash of a file and print `blake3:<hexdigest>`.
/// Used to generate integrity hashes for `$include` second arguments.
// AMBIENT-OK: CLI hash command on operator-specified file.
#[allow(clippy::disallowed_types)]
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
///
/// Returns an `Arc<SourceFile>` with both the path and content, ready to be
/// threaded into `parse_with_file` so that all spans in the parsed AST carry
/// a reference to the originating source file.
// AMBIENT-OK: CLI entry point reading operator-specified file.
#[allow(clippy::disallowed_types)]
fn read_source(file_path: &str) -> Result<Arc<SourceFile>, String> {
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
        Ok(Arc::new(SourceFile {
            path: Arc::from("-"),
            content: Arc::from(buf.as_str()),
        }))
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
        Ok(Arc::new(SourceFile {
            path: Arc::from(file_path),
            content: Arc::from(buf.as_str()),
        }))
    }
}

/// Configuration for literate mode operations.
struct LiterateConfig<'a> {
    file_path: &'a str,
    mode: &'a LiterateMode,
    strict: bool,
}

/// Process a Markdown file in literate mode.
///
/// Extracts ```` ```tinct ```` and ```` ```llt ```` fenced code blocks and
/// handles them according to `mode`:
///
/// - **`tangle`** — print the extracted blocks joined with `\n---\n`.
// AMBIENT-OK: CLI bootstrap — opens file parent dir for evaluation
#[allow(clippy::disallowed_methods)]
async fn run_literate(config: &LiterateConfig<'_>) -> Result<(), String> {
    let file_path = config.file_path;
    let markdown = String::from(&*read_source(file_path)?.content);
    let blocks = literate::extract_code_blocks(&markdown);

    if blocks.is_empty() {
        match config.mode {
            LiterateMode::Tangle => {
                // Nothing to print — output empty string (no trailing newline).
                return Ok(());
            }
            LiterateMode::Lint => {
                // Nothing to lint — clean exit.
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

        LiterateMode::Lint => {
            let tangled = literate::tangle(blocks);
            run_literate_lint(&tangled, config).await
        }
    }
}

/// Lint mode: type-check tangled tinct source without evaluating.
///
/// Extracts code blocks from Markdown, tangles them into a single pipeline source,
/// parses and type-checks the result, then reports type errors and warnings to stderr.
///
/// Exit codes:
/// - 0 if no errors (warnings allowed without --strict)
/// - 1 if type errors found, or if warnings found with --strict
///
/// The base directory is derived from the Markdown file's parent directory.
/// AMBIENT-OK: CLI literate-lint opening markdown file directory for include resolution.
#[allow(clippy::disallowed_methods)]
async fn run_literate_lint(tangled: &str, config: &LiterateConfig<'_>) -> Result<(), String> {
    let markdown_path = config.file_path;
    let strict = config.strict;

    // Parse the tangled source.
    let output = parse(tangled).map_err(|e| {
        if strict {
            tinct::format_parse_error(&e, tangled, markdown_path)
        } else {
            format!("parse error in tangled tinct source: {e}")
        }
    })?;

    // PIPELINE INVARIANT: parse -> desugar -> resolve -> typecheck.
    let mut program = output.program;
    tinct::desugar::desugar_surface_program(&mut program);
    // Transform instance decls to method dicts (T-1142).
    tinct::desugar::desugar_instance_decls_surface_program(&mut program);
    let env_arc = tinct::get_builtin_core_type_env()
        .await
        .expect("builtin core type env unavailable");
    let (type_errors, _type_map, _doc_map, _scheme_map, diagnostics) =
        tinct::typecheck::typecheck_surface_program(&program, env_arc).await;

    // Collect all errors and warnings
    let mut all_messages = Vec::new();

    for e in &type_errors {
        all_messages.push(tinct::format_type_error(e, tangled, markdown_path));
    }

    // In --strict mode, bump each diagnostic's level before display (Info→Warn, Warn→Err),
    // then treat Err-level diagnostics as fatal. Mirrors run_eval and run_fmt behavior.
    let mut has_fatal_diag = false;
    for d in &diagnostics {
        let effective = if strict {
            let bumped_level = d.level.bump();
            if bumped_level == tinct::DiagnosticLevel::Err {
                has_fatal_diag = true;
            }
            tinct::TypeDiagnostic {
                level: bumped_level,
                message: d.message.clone(),
                span: d.span.clone(),
                code: d.code,
            }
        } else {
            d.clone()
        };
        all_messages.push(format_type_diagnostic(&effective, tangled, markdown_path));
    }

    // Type errors always fatal; diagnostics (warnings) fatal only with --strict (at Err level)
    let fatal_count = if strict {
        type_errors.len() + if has_fatal_diag { 1 } else { 0 }
    } else {
        type_errors.len()
    };

    if !all_messages.is_empty() {
        eprintln!("{}", all_messages.join("\n"));
    }

    if fatal_count > 0 {
        return Err(format!("lint failed with {} issue(s)", fatal_count));
    }

    // Clean — exit 0 (no output on success)
    Ok(())
}

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

/// Collects the human-readable contract information for one document section.
struct ContractSection {
    section_idx: usize,
    type_name: Option<String>,
    fields: Vec<(String, String)>, // (field_name, constraint_description)
    schema: Vec<(String, String)>, // (field_name, constraint_description)
    docs: Vec<(String, String)>,   // (binding_name, doc_string)
}

/// Describe the input contract of an LLT file.
///
/// Parses the file, extracts `%@Type` / `expects:` annotations from each document,
/// and detects schema dicts by heuristic. Outputs a human-readable summary.
// AMBIENT-OK: CLI describe — opens file parent dir for type-checking
#[allow(clippy::disallowed_methods)]
async fn run_describe(file_path: &str) -> Result<(), String> {
    let sf = read_source(file_path)?;
    let source = String::from(&*sf.content);
    let output = parse_with_file(&source, Arc::clone(&sf)).map_err(|e| format!("{e}"))?;

    // PIPELINE INVARIANT: parse -> desugar -> resolve -> typecheck.
    let mut program = output.program;
    tinct::desugar::desugar_surface_program(&mut program);
    // Type check to get DocMap (for doc strings).
    let env_arc = tinct::get_builtin_core_type_env()
        .await
        .expect("builtin core type env unavailable");
    let (_type_errors, _type_map, doc_map, _scheme_map, _diagnostics) =
        tinct::typecheck::typecheck_surface_program(&program, env_arc).await;

    // Collect contract information from each document section.
    let mut contracts: Vec<ContractSection> = Vec::new();
    let mut has_any_contract = false;

    for (doc_idx, doc) in program.documents.iter().enumerate() {
        let mut type_name: Option<String> = None;
        let mut fields: Vec<(String, String)> = Vec::new();

        // Extract expects: / %@Type annotation
        if let Some(ref ann) = doc.node.expects {
            has_any_contract = true;
            match &ann.node {
                tinct::Annotation::Simple(name) => {
                    type_name = Some(name.clone());
                }
                tinct::Annotation::PropertyDict(entries) => {
                    for entry in entries {
                        if let Some(ref key_node) = entry.node.key {
                            if let tinct::SurfaceExpression::Str(ref key_name) = key_node.expr {
                                fields.push((
                                    key_name.clone(),
                                    describe_surface_annotation_value(&entry.node.value.expr),
                                ));
                            }
                        }
                    }
                }
                tinct::Annotation::Annotated(name, _inner) => {
                    type_name = Some(name.clone());
                }
            }
        }

        // Detect schema dicts in the document expressions
        let schema = detect_schema_dict(&doc.node);
        if !schema.is_empty() {
            has_any_contract = true;
        }

        // Include doc strings from DocMap for top-level bindings
        let docs = extract_doc_strings_from_doc(&doc.node, &doc_map);
        if !docs.is_empty() {
            has_any_contract = true;
        }

        let has_content =
            type_name.is_some() || !fields.is_empty() || !schema.is_empty() || !docs.is_empty();
        if has_content {
            contracts.push(ContractSection {
                section_idx: doc_idx,
                type_name,
                fields,
                schema,
                docs,
            });
        }
    }

    if !has_any_contract {
        println!("no input contract");
        return Ok(());
    }

    // Human-readable output: one line per field, with doc strings
    for contract in &contracts {
        if contracts.len() > 1 {
            println!("--- section {} ---", contract.section_idx);
        }
        if let Some(ref name) = contract.type_name {
            println!("  expects: @{}", name);
        }
        for (name, constraint) in &contract.fields {
            if let Some((_, doc_str)) = contract.docs.iter().find(|(k, _)| k == name) {
                println!("  {}: {} — {}", name, constraint, doc_str);
            } else {
                println!("  {}: {}", name, constraint);
            }
        }
        for (name, constraint) in &contract.schema {
            if let Some((_, doc_str)) = contract.docs.iter().find(|(k, _)| k == name) {
                println!("  {}: {} — {}", name, constraint, doc_str);
            } else {
                println!("  {}: {}", name, constraint);
            }
        }
        // Show doc strings for bindings not in fields/schema
        let field_names: std::collections::HashSet<&str> =
            contract.fields.iter().map(|(k, _)| k.as_str()).collect();
        let schema_names: std::collections::HashSet<&str> =
            contract.schema.iter().map(|(k, _)| k.as_str()).collect();
        for (name, doc_str) in &contract.docs {
            if !field_names.contains(name.as_str()) && !schema_names.contains(name.as_str()) {
                println!("  {} — {}", name, doc_str);
            }
        }
    }

    Ok(())
}

/// Extract doc strings from a document's top-level bindings.
/// Scans dict expressions in the document for entries that have doc strings in the DocMap.
fn extract_doc_strings_from_doc(
    doc: &tinct::ast::SurfaceDocument,
    doc_map: &std::collections::HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut result: Vec<(String, String)> = Vec::new();

    for expr in doc.expressions() {
        if let tinct::ast::SurfaceExpression::Dict(entries) = &expr.expr {
            for entry in entries {
                if let Some(ref key_node) = entry.node.key {
                    // Extract the binding name from the key expression
                    // Keys can be:
                    // - SurfaceExpression::Str (string literal key)
                    // - SurfaceExpression::Annotated { name, .. } (annotated binding like name@[...])
                    // - SurfaceExpression::VarRef (bare identifier key)
                    // Both plain and annotated VarRef use the name field.
                    let name_opt = match &key_node.expr {
                        tinct::ast::SurfaceExpression::Str(s) => Some(s.as_str()),
                        tinct::ast::SurfaceExpression::VarRef { name, .. } => Some(name.as_str()),
                        _ => None,
                    };

                    if let Some(name) = name_opt {
                        if let Some(doc_str) = doc_map.get(name) {
                            result.push((name.to_string(), doc_str.clone()));
                        }
                    }
                }
            }
        }
    }

    result
}

/// Detect schema dicts in a document's expressions.
///
/// A dict is a schema dict if any of its values is itself a dict containing
/// at least one recognized schema key (type, min, max, min-length, max-length,
/// pattern, required, items, fields, enum).
fn detect_schema_dict(doc: &tinct::ast::SurfaceDocument) -> Vec<(String, String)> {
    let mut result: Vec<(String, String)> = Vec::new();
    for expr in doc.expressions() {
        if let tinct::ast::SurfaceExpression::Dict(entries) = &expr.expr {
            for entry in entries {
                if let Some(ref key_node) = entry.node.key {
                    if let tinct::ast::SurfaceExpression::Str(ref field_name) = key_node.expr {
                        // Check if the value is a dict with schema keys
                        if let Some(constraint_str) = extract_schema_info(&entry.node.value.expr) {
                            result.push((field_name.clone(), constraint_str));
                        }
                    }
                }
            }
        }
    }
    result
}

/// If `expr` is a dict containing at least one recognized schema key, return
/// a human-readable constraint string. Otherwise return None.
fn extract_schema_info(expr: &tinct::ast::SurfaceExpression) -> Option<String> {
    if let tinct::ast::SurfaceExpression::Dict(entries) = expr {
        let mut parts: Vec<String> = Vec::new();
        let mut has_schema_key = false;
        for entry in entries {
            if let Some(ref key_node) = entry.node.key {
                if let tinct::ast::SurfaceExpression::Str(ref key_name) = key_node.expr {
                    if SCHEMA_KEYS.contains(&key_name.as_str()) {
                        has_schema_key = true;
                        let val_str = describe_surface_annotation_value(&entry.node.value.expr);
                        parts.push(format!("{key_name}: {val_str}"));
                    }
                }
            }
        }
        if has_schema_key {
            return Some(parts.join(", "));
        }
    }
    None
}

/// Turn a surface annotation value expression into a human-readable string.
fn describe_surface_annotation_value(expr: &tinct::ast::SurfaceExpression) -> String {
    match expr {
        tinct::ast::SurfaceExpression::Str(s) => s.clone(),
        tinct::ast::SurfaceExpression::Int(n) => n.to_string(),
        tinct::ast::SurfaceExpression::U64(n) => n.to_string(),
        tinct::ast::SurfaceExpression::Float(f) => f.to_string(),
        tinct::ast::SurfaceExpression::VarRef { name, .. } => name.clone(),
        _ => "(complex)".to_string(),
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

        "T018" => {
            "\
T018: Undefined constructor in structural test (type checker)

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

        "T019" => {
            "\
T019: Nullary constructor used as payload-binding structural test (type checker)

A [case [let v: ConstructorName] body] arm attempts to bind v to the payload
of a nullary constructor (one that carries no value). Since a nullary constructor
has no payload, v can never be bound, and this arm is structurally dead.

Fix: use [let _: ConstructorName] to match the constructor tag without binding,
or use a constructor that carries a payload if you need to extract a value."
        }

        "T020" => {
            "\
T020: Dead match arm — pattern type disjoint from scrutinee type (type checker)

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

        "T021" => {
            "\
T021: Unknown type parameter annotation (type checker)

A type alias declaration uses an `@X` annotation on a type parameter where X is
neither a recognized variance keyword (Covariant, Contravariant, Invariant, Phantom)
nor a registered type class name.

Fix: use a valid variance annotation or a registered class name (e.g., @Covariant, @Equatable)."
        }

        _ => {
            return Err(format!(
                "unknown error code: {code}\n\
                 Run 'tinct explain <code>' with a valid code, e.g. E001 through E099 or T000-T021.\n\
                 Known codes: E001, E002, E010, E011, E020-E024, E030-E036, \
                 E043-E044, E051-E056, E060, E063, E070, E080, E090, E099, \
                 T000, T001, T002, T003, T004, T017, T018, T019, T020, T021."
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
