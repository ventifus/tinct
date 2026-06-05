//! LLT command-line tool: parses and evaluates `.llt` files, outputs JSON or LLT display format.

#![deny(clippy::disallowed_types, clippy::disallowed_methods)]
// Arc<Thunk> and related types are !Send because Thunk contains Rc<...>. LLT uses
// tokio::task::LocalSet with a current_thread runtime — values never cross threads.
#![allow(clippy::arc_with_non_send_sync)]

use clap::{Parser, Subcommand, ValueEnum};
use std::io::{self, Read};
use std::path::PathBuf;
use std::process;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use tinct::{
    create_stdlib_env, escape_json_str, literate, materialize_sync as materialize, parse,
    parse_with_file, visit_value, EvalContext, JsonVisitor, SourceFile, Thunk, MAX_FILE_SIZE,
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
    /// Extract blocks, evaluate the pipeline, and print the result as JSON.
    Eval,
    /// Evaluate blocks and output the Markdown with JSON results as comments after each block.
    Weave,
    /// Extract blocks, type-check without evaluating.
    Lint,
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
    // Install ring as the process-level TLS crypto provider.
    // Both ring and aws-lc-rs are compiled in (via quinn+reqwest feature flags);
    // rustls panics at runtime if the process default is ambiguous.
    // quinn already requires ring, so ring is the consistent choice.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();

    // Apply resource limits globally (before subcommand dispatch).
    // Skipped in debug builds: the default 512 MB RLIMIT_AS causes OOM when
    // running CLI tests under `cargo test` (debug mode uses more virtual memory).
    #[cfg(all(unix, not(debug_assertions)))]
    if let Err(e) = setup_rlimits(cli.max_memory, cli.max_cpu, cli.max_fds) {
        eprintln!("error: {e}");
        process::exit(EXIT_ERROR);
    }
    // On non-Unix platforms (or debug builds), rlimit flags are accepted for CLI
    // compatibility but have no effect.
    #[cfg(any(not(unix), debug_assertions))]
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
    // The REPL spawns its own 128MB thread for eval when needed (src/repl.rs).
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
            expr,
            input,
            output,
            profile,
            files,
        } => run_eval(
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
            expr,
            input,
            output,
            profile.as_deref(),
        ),
        Commands::Hash { file } => run_hash(&file),
        Commands::Fmt {
            check,
            in_place,
            output,
            strict,
            file,
        } => tinct::async_rt::block_on_anywhere(run_fmt(&file, check, in_place, &output, strict)),
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
            strict,
        } => run_lint(&file, no_fs, strict, &cap_fs, &cap_net),
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
        } => run_literate(&LiterateConfig {
            file_path: &file,
            mode: &mode,
            no_substitute,
            strict,
            in_place,
            verify,
            fail_on_errors,
            cap_fs: &cap_fs,
            cap_net: &cap_net,
        }),
    };

    match result {
        Ok(()) => {}
        Err(e) => {
            eprintln!("{e}");
            process::exit(EXIT_ERROR);
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
/// Default values are applied when the caller passes `None`:
/// - `max_memory`: 512 MB RLIMIT_AS limit (controls virtual address space; also
///   caps the maximum heap size the process can mmap).
/// - `max_cpu`: No limit by default (must be explicitly requested).
/// - `max_fds`: 64 RLIMIT_NOFILE (prevents FD exhaustion from crafted $include
///   chains; still leaves room for stdin/stdout/stderr + eval fds).
///
/// A value of `Some(0)` disables that particular limit.
#[cfg(all(unix, not(debug_assertions)))]
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
fn run_eval(
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
    expr: Vec<String>,
    input: Option<String>,
    output: Option<String>,
    profile: Option<&str>,
) -> Result<(), String> {
    // Build the complete pipeline: [input formatter] + [files/exprs interleaved] + [output formatter]
    let mut pipeline_stages: Vec<PipelineStage> = Vec::new();

    // Prepend -i input formatter if specified
    if let Some(ref input_format) = input {
        // Validate formatter name: only alphanumeric and hyphens allowed.
        // This prevents path traversal via -i ../../secret or similar.
        if !input_format
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-')
        {
            return Err(format!(
                "--input: invalid formatter name {:?} (only alphanumeric and '-' allowed)",
                input_format
            ));
        }
        let libdir_path = resolve_libdir_path(libdir_path.as_deref())
            .ok_or_else(|| "--input: stdlib directory not found (libdir)".to_string())?;
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
            input_path
                .to_str()
                .ok_or_else(|| "formatter path is not valid UTF-8".to_string())?
                .to_string(),
        ));
    }

    // Interleave files and -e expressions in the order they appear on the CLI.
    // Clap doesn't preserve mixed positional/flag order, so we reconstruct it
    // by parsing raw CLI arguments.
    let interleaved_stages = interleave_files_and_exprs(file_paths, &expr);
    pipeline_stages.extend(interleaved_stages);

    // Append -o output formatter if specified
    if let Some(ref output_format) = output {
        // Validate formatter name: only alphanumeric and hyphens allowed.
        // This prevents path traversal via -o ../../secret or similar.
        if !output_format
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-')
        {
            return Err(format!(
                "--output: invalid formatter name {:?} (only alphanumeric and '-' allowed)",
                output_format
            ));
        }
        let libdir_path = resolve_libdir_path(libdir_path.as_deref())
            .ok_or_else(|| "--output: stdlib directory not found (libdir)".to_string())?;
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
            output_path
                .to_str()
                .ok_or_else(|| "formatter path is not valid UTF-8".to_string())?
                .to_string(),
        ));
    } else {
        // When no -o flag is specified, use none.llt as the default output formatter.
        // This drains %emit and forces % without writing any output.
        // See doc/whatif/data-streaming.md §src/main.rs for the spec.
        let libdir_path = resolve_libdir_path(libdir_path.as_deref()).ok_or_else(|| {
            "stdlib directory not found (libdir) - needed for default output formatter".to_string()
        })?;
        let none_path = libdir_path.join("cli").join("out").join("none.llt");
        if !none_path.exists() {
            return Err(format!(
                "internal error: default output formatter not found: {} (stdlib installation is broken)",
                none_path.display()
            ));
        }
        pipeline_stages.push(PipelineStage::File(
            none_path
                .to_str()
                .ok_or_else(|| "none.llt path is not valid UTF-8".to_string())?
                .to_string(),
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

    // Resource limits are now applied globally in main() before subcommand dispatch.

    // Create stdlib environment
    let env = create_stdlib_env().map_err(|e| format!("{e}"))?;
    // Build type-stage environment (for builtin_eval_types). Falls back to stdlib_env if unavailable.
    let type_stage_env = tinct::build_type_stage_env().unwrap_or_else(|| Arc::clone(&env));

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
        let cwd_thunk = tinct::Thunk::new_materialized(cwd_value, tinct::Span::origin());
        env.write()
            .unwrap()
            .insert("%cwd".to_string(), Arc::new(cwd_thunk));
    }

    // Inject `%stdin` Handle for fd 0 into the root environment only when `-i` is present.
    // When `-i` is not specified, there is no stdin input.
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
        env.write()
            .unwrap()
            .insert("%stdin".to_string(), Arc::new(stdin_thunk));
    }

    // Inject `%stdout` WriteHandle into the root environment.
    // Output formatters write directly to %stdout via [write-handle %stdout ...].
    // This replaces the old "formatter returns String, CLI prints it" model.
    {
        use std::cell::RefCell;
        use std::collections::HashMap;
        use std::io::BufWriter;
        use tinct::Value;

        // Create stdout WriteHandle with default caps (Bool(true) sentinel, consistent with stdin)
        let mut caps = HashMap::new();
        caps.insert("Writable".to_string(), Value::Bool(true));
        caps.insert("Text".to_string(), Value::Bool(true));

        let stdout_handle = Value::WriteHandle {
            caps,
            inner: Rc::new(RefCell::new(
                Box::new(BufWriter::new(std::io::stdout())) as Box<dyn std::io::Write>
            )),
        };
        let stdout_thunk = tinct::Thunk::new_materialized(stdout_handle, tinct::Span::origin());
        env.write()
            .unwrap()
            .insert("%stdout".to_string(), Arc::new(stdout_thunk));
    }

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
                let libdir_thunk =
                    tinct::Thunk::new_materialized(libdir_value, tinct::Span::origin());
                env.write()
                    .unwrap()
                    .insert("%libdir".to_string(), Arc::new(libdir_thunk));
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
            let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
            // Inject as `%NAME` (auto-prefix %).
            let scoped_name = if name.starts_with('%') {
                name.to_string()
            } else {
                format!("%{name}")
            };
            env.write()
                .unwrap()
                .insert(scoped_name, Arc::new(cap_thunk));
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
            env.write().unwrap().insert(name, Arc::new(cap_thunk));
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
        env.write()
            .unwrap()
            .insert("%clock".to_string(), Arc::new(cap_thunk));
    }

    // Inject --cap-file NAME=PATH[:MODE] entries into the root environment as `%NAME`.
    // --no-fs suppresses all cap-file entries (filesystem access is blocked globally).
    // AMBIENT-OK: CLI bootstrap — operator-specified file paths via --cap-file.
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
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
            env.write()
                .unwrap()
                .insert(scoped_name, Arc::new(cap_thunk));
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

    // Multi-stage pipeline: parse/expand/typecheck all stages, then invoke eval-programs once.
    //
    // ARENA SHARING INVARIANT: All stages in the pipeline must share the same ThunkArena so
    // that ThunkIds allocated by earlier stages remain valid when later stages reference them
    // via the `%` pipeline variable. We establish one base EvalContext for the first stage,
    // then use `with_base_dir_and_path` for subsequent stages — this creates a new config
    // (different base_dir) while sharing the same arena, state, and stdlib_env.
    // eval-programs threads % through each Value::Program in sequence.

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

    let mut last_source = String::new();
    let mut last_eval_ctx: Option<Arc<EvalContext>> = None;
    let mut base_eval_ctx: Option<Arc<EvalContext>> = None;

    // Collect Value::Program values from each pipeline stage (parse → expand → resolve → typecheck).
    // Evaluation is deferred until all programs are collected — eval-programs threads % through them.
    let mut collected_programs: Vec<tinct::Value> = Vec::new();

    // Wrap the pipeline loop and output section in a closure so we can run profile
    // write + flush thread cleanup unconditionally regardless of success or failure.
    // The closure captures all mutable locals by mutable reference; after it returns,
    // `last_eval_ctx` is available for the final profile write.
    let eval_result: Result<(), String> = (|| {
        for stage in &pipeline_stages {
            // Read the LLT source (from file or inline expression).
            // For file stages, build an Arc<SourceFile> so spans carry the file name.
            // For inline expressions, parse without a file reference.
            // `source` is kept for downstream error formatting (type errors, eval snippets).
            let (source, output) = match stage {
                PipelineStage::File(file_path) => {
                    let sf = read_source(file_path)?;
                    let source_str = String::from(&*sf.content);
                    let out = parse_with_file(&source_str, Arc::clone(&sf)).map_err(|e| {
                        if strict {
                            tinct::format_parse_error(&e, &source_str, file_path)
                        } else {
                            format!("{e}")
                        }
                    })?;
                    (source_str, out)
                }
                PipelineStage::Expr(expression) => {
                    let out = parse(expression).map_err(|e| {
                        if strict {
                            tinct::format_parse_error(&e, expression, "<expr>")
                        } else {
                            format!("{e}")
                        }
                    })?;
                    (expression.clone(), out)
                }
            };

            // Determine base directory for $include resolution (needed for expand, typecheck, and eval).
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

            // Open base_dir as a cap-std Dir before expand_surface_program so it can be passed in
            // without re-acquiring ambient authority inside the expansion step.
            let base_dir = cap_std::fs::Dir::open_ambient_dir(
                &file_base_dir_path,
                cap_std::ambient_authority(),
            )
            .map_err(|e| format!("cannot open base directory: {e}"))?;

            // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> resolve_surface_program -> typecheck -> eval.
            // Use expand_surface_program (not expand_macros) so SurfaceItem::Decl macros are seen.
            // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
            // See also: src/lib.rs (eval_source_with_config pipeline), src/expand.rs module comment.
            let mut program = output.program;
            tinct::async_rt::block_on_anywhere(tinct::expand::expand_surface_program(
                &mut program,
                no_fs,
                &base_dir,
            ))
            .map_err(|e| format!("{e}"))?;
            // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
            tinct::desugar::desugar_surface_program(&mut program);
            // Inject ADT constructor bindings (must run after desugar, before resolve).
            tinct::desugar::inject_adt_constructors_surface_program(&mut program);
            // Variable resolution pass (Phase 1 of arena allocation strategy).
            let resolution_table =
                std::sync::Arc::new(tinct::resolve::resolve_surface_program(&program));
            // Type errors are advisory unless --strict is set.
            // Build type environment with prelude + includes (if file-based).
            let type_env = match stage {
                PipelineStage::File(file_path) if file_path != "-" => {
                    // File-based: use build_type_env with base_dir for include resolution
                    let (env, _include_bindings) =
                        tinct::build_type_env(&program, Some(&file_base_dir_path));
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
                type_annotation_table_from_env,
            ) = tinct::typecheck::typecheck_surface_program_with_env(
                &program, type_env, false, // disable scheme_map (not needed for eval)
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
            // In --strict mode, bump each diagnostic's level and treat Err-level diagnostics
            // as fatal (they escalate Info→Warn→Err under --strict).
            if !type_diagnostics.is_empty() {
                let diag_file_name = match stage {
                    PipelineStage::File(fp) => fp.as_str(),
                    PipelineStage::Expr(_) => "<expr>",
                };
                let mut has_fatal_diag = false;
                for d in &type_diagnostics {
                    let effective = if strict {
                        use tinct::DiagnosticLevel;
                        let bumped_level = d.level.bump();
                        // Emit with the bumped level
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
                    eprintln!(
                        "{}",
                        format_type_diagnostic(&effective, &source, diag_file_name)
                    );
                }
                if strict && has_fatal_diag {
                    return Err(
                        "type checking failed — type warnings escalated to errors by --strict"
                            .to_string(),
                    );
                }
            }

            // Create or derive the evaluation context.
            // First file: create the base context (owns the ThunkArena).
            // Subsequent files: derive from the base context via with_base_dir_and_path so all
            // files share the same arena — ThunkIds from earlier files remain valid in later ones.
            let stage_source_file = match stage {
                PipelineStage::File(fp) if fp != "-" => Some(fp.clone()),
                _ => None,
            };
            let eval_ctx = if let Some(ref base) = base_eval_ctx {
                let mut ctx =
                    base.with_base_dir_and_path(base_dir, Some(file_base_dir_path.clone()));
                // Update source file for this stage so backtrace frames show the right filename.
                Arc::get_mut(&mut ctx)
                    .unwrap()
                    .set_source_file(stage_source_file);
                ctx
            } else {
                let mut ctx = EvalContext::new_with_options(
                    base_dir,
                    Arc::clone(&env),
                    Arc::clone(&type_stage_env),
                    no_fs,
                    require_integrity,
                    env_allowed.clone(),
                );
                // Set profiling collector if --profile was specified
                if let Some(ref collector) = profiling_collector {
                    Arc::get_mut(&mut ctx).unwrap().profiling = Some(Arc::clone(collector));
                }
                // Set source file for backtrace frame filenames.
                Arc::get_mut(&mut ctx)
                    .unwrap()
                    .set_source_file(stage_source_file);
                // Share the already-open libdir Dir with the evaluator so that the self-hosted
                // `include` (prelude.llt) can inject %libdir into nested includes without re-acquiring ambient authority.
                if let Some(ref libdir_rc) = libdir_rc_for_ctx {
                    ctx.set_libdir_dir(Arc::clone(libdir_rc));
                }
                base_eval_ctx = Some(Arc::clone(&ctx));
                ctx
            };

            // Wire boundary guards and do-infer resolutions from type inference to the eval context
            eval_ctx.set_boundary_guards(infer_state.boundary_guards);
            eval_ctx.set_do_infer_resolutions(infer_state.do_infer_resolutions);
            eval_ctx.set_tycon_env(infer_state.tycon_env);

            // TypeAnnotationTable was populated directly by typecheck_surface_program_with_env
            // above — no second typecheck call needed.
            let type_annotation_table = std::sync::Arc::new(type_annotation_table_from_env);

            // Collect this stage as a Value::Program for eval-programs.
            // eval-programs (in loader.llt) threads % through each program in sequence.
            collected_programs.push(tinct::Value::Program {
                program: std::sync::Arc::new(program),
                resolutions: resolution_table,
                types: type_annotation_table,
                expects_resolved: std::sync::Arc::new(infer_state.expects_resolved),
            });

            last_source = source;
            last_eval_ctx = Some(eval_ctx);
        }

        if collected_programs.is_empty() {
            return Err("internal error: no programs collected".to_string());
        }

        let eval_ctx = last_eval_ctx
            .clone()
            .ok_or_else(|| "internal error: no eval context".to_string())?;

        // Look up eval-programs from the stdlib env (exported by loader.llt → prelude).
        // eval-programs: [fn [let programs initial-input] ...]
        // It threads % through each program in sequence, returning the final output.
        let eval_programs_thunk = {
            let env_guard = env.read().unwrap();
            env_guard
                .get("eval-programs")
                .ok_or_else(|| {
                    "internal error: eval-programs not found in stdlib env (prelude not loaded?)"
                        .to_string()
                })?
                .clone()
        };
        let eval_programs_val = materialize(&eval_programs_thunk, None, &eval_ctx)
            .map_err(|e| format!("internal error: failed to materialize eval-programs: {e}"))?;

        // Build a Seq.Cons/Seq.Nil list of programs from the collected programs (right-folded).
        // Build from the end so the first program is at the head.
        let mut seq_thunk = Arc::new(tinct::Thunk::new_materialized(
            tinct::make_seq_nil(),
            tinct::Span::origin(),
        ));
        for prog_val in collected_programs.into_iter().rev() {
            let head_thunk = Arc::new(tinct::Thunk::new_materialized(
                prog_val,
                tinct::Span::origin(),
            ));
            let head_id = eval_ctx.alloc_thunk(head_thunk);
            let tail_id = eval_ctx.alloc_thunk(seq_thunk);
            seq_thunk = Arc::new(tinct::Thunk::new_materialized(
                tinct::make_seq_cons(head_id, tail_id, &eval_ctx),
                tinct::Span::origin(),
            ));
        }

        // The initial input to the pipeline (% for the first program).
        // None → empty dict (same default as eval_surface_file_with_input).
        let initial_input_thunk = Arc::new(tinct::Thunk::new_materialized(
            tinct::Value::Dict(indexmap::IndexMap::new()),
            tinct::Span::origin(),
        ));

        // Invoke eval-programs([programs_seq, initial_input]) via invoke_function.
        // eval-programs is a two-argument function: [fn [let programs initial-input] ...]
        let programs_arg = seq_thunk;
        let input_arg = initial_input_thunk;

        match eval_programs_val {
            tinct::Value::Function {
                ref params,
                ref body,
                env: ref closure_env,
                ..
            } => {
                let positional = vec![programs_arg, input_arg];
                let call_ctx = tinct::CallContext {
                    params: params.as_slice(),
                    body,
                    closure_env,
                    positional: &positional,
                    named: None,
                    default_env: closure_env,
                    call_span: tinct::Span::origin(),
                    origin: Some(Arc::from("eval-programs")),
                    ctx: &eval_ctx,
                };
                // invoke_function returns an unevaluated thunk (the function body is lazy).
                // Materialize it to drive the full pipeline to completion, including the
                // output formatter's drain task and [await drain]. Without materialization,
                // eval-programs is a no-op and no output is ever produced.
                tinct::async_rt::block_on(async {
                    let thunk = tinct::invoke_function(&call_ctx).await?;
                    tinct::materialize(&thunk, None, &eval_ctx).await
                })
                .map_err(|e| {
                    let mut error_str = format!("{e}");
                    if let Some(snippet) =
                        tinct::render_span_snippet(&last_source, e.definition_span)
                    {
                        error_str.push('\n');
                        error_str.push_str(&snippet);
                    }
                    error_str
                })?;

                // Flush %stdout explicitly. BufWriter::drop does not run when
                // process::exit is called (SIGINT path or EXIT_ERROR). The `?`
                // above short-circuits on eval error, so this flush only runs on
                // the success path — avoiding emission of corrupt partial output.
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            other => {
                return Err(format!(
                    "internal error: eval-programs is {} instead of Function",
                    other.type_name()
                ));
            }
        }

        Ok(())
    })(); // end of eval_result closure

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

        // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> resolve -> typecheck.
        // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
        let fmt_base_dir = open_file_base_dir(file_path, "fmt")?;
        // Use expand_surface_program (not expand_macros) so SurfaceItem::Decl macros are seen.
        let mut program = output.program;
        tinct::async_rt::block_on_anywhere(tinct::expand::expand_surface_program(
            &mut program,
            false,
            &fmt_base_dir,
        ))
        .map_err(|e| format!("{e}"))?;
        // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
        tinct::desugar::desugar_surface_program(&mut program);
        // Variable resolution pass (Phase 1 of arena allocation strategy).
        let _resolution_table = tinct::resolve::resolve_surface_program(&program);
        let env = tinct::build_prelude_env();
        let (type_errors, _type_map, _doc_map, _scheme_map, fmt_diagnostics) =
            tinct::typecheck::typecheck_surface_program(&program, env);

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
fn run_lint(
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

    // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> resolve -> typecheck.
    // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
    // AMBIENT-OK: CLI bootstrap — operator specified this file path.
    let lint_base_dir = {
        let p = std::path::Path::new(file_path);
        let dir = p
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        cap_std::fs::Dir::open_ambient_dir(dir, cap_std::ambient_authority())
            .map_err(|e| format!("cannot open base directory for lint: {e}"))?
    };
    // Use expand_surface_program (not expand_macros) so SurfaceItem::Decl macros are seen.
    let mut program = output.program;
    tinct::async_rt::block_on_anywhere(tinct::expand::expand_surface_program(
        &mut program,
        false,
        &lint_base_dir,
    ))
    .map_err(|e| format!("{e}"))?;
    // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
    tinct::desugar::desugar_surface_program(&mut program);
    // Variable resolution pass (Phase 1 of arena allocation strategy).
    let _resolution_table = tinct::resolve::resolve_surface_program(&program);
    // Type check with prelude environment
    let env = tinct::build_prelude_env();
    let (type_errors, _type_map, _doc_map, _scheme_map, diagnostics) =
        tinct::typecheck::typecheck_surface_program(&program, env);

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
    no_substitute: bool,
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
/// - **`eval`** — join the blocks, evaluate the resulting pipeline, print JSON.
/// - **`weave`** — evaluate each block in pipeline order; output the original
///   Markdown with the JSON result appended as a comment after each tinct block.
// AMBIENT-OK: CLI bootstrap — opens file parent dir for evaluation
#[allow(clippy::disallowed_methods)]
fn run_literate(config: &LiterateConfig) -> Result<(), String> {
    let file_path = config.file_path;
    let markdown = String::from(&*read_source(file_path)?.content);
    let blocks = literate::extract_code_blocks(&markdown);

    if blocks.is_empty() {
        match config.mode {
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

        LiterateMode::Eval => {
            let tangled = literate::tangle(blocks);
            run_literate_eval(&tangled, config)
        }

        LiterateMode::Weave => run_literate_weave(&markdown, &blocks, config),

        LiterateMode::Lint => {
            let tangled = literate::tangle(blocks);
            run_literate_lint(&tangled, config)
        }
    }
}

/// Evaluate a tangled tinct source string and print the result as JSON.
///
/// Always serializes output to JSON regardless of any `-o` formatter flag.
/// The `-o` flag is respected only by `run_eval`; literate eval always uses
/// the JSON serialization path (parse → desugar → resolve → typecheck →
/// eval → materialize → JSON).
/// The base directory is derived from the Markdown file's parent directory.
///
/// Literate mode always runs with --no-cwd and --no-env (hard-coded).
/// Capabilities are injected via cap_fs and cap_net. %libdir is always available.
/// %clock is set to a fixed ClockCap from the markdown file's mtime.
// AMBIENT-OK: CLI literate-eval reading markdown file metadata for mtime.
#[allow(clippy::disallowed_methods)]
fn run_literate_eval(tangled: &str, config: &LiterateConfig) -> Result<(), String> {
    let markdown_path = config.file_path;
    let strict = config.strict;
    let cap_fs = config.cap_fs;
    let cap_net = config.cap_net;
    // Parse the tangled source.
    let output = parse(tangled).map_err(|e| {
        if strict {
            tinct::format_parse_error(&e, tangled, markdown_path)
        } else {
            format!("parse error in tangled tinct source: {e}")
        }
    })?;

    // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> resolve -> typecheck.
    // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
    let weave_base_dir = open_file_base_dir(markdown_path, "weave")?;
    // Use expand_surface_program (not expand_macros) so SurfaceItem::Decl macros are seen.
    let mut program = output.program;
    tinct::async_rt::block_on_anywhere(tinct::expand::expand_surface_program(
        &mut program,
        false,
        &weave_base_dir,
    ))
    .map_err(|e| format!("{e}"))?;
    // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
    tinct::desugar::desugar_surface_program(&mut program);
    // Variable resolution pass (Phase 1 of arena allocation strategy).
    let resolution_table = std::sync::Arc::new(tinct::resolve::resolve_surface_program(&program));
    let (type_errors, type_annotation_table, expects_resolved) =
        tinct::typecheck::typecheck_surface_program_annotation_table(&program);
    let type_annotation_table = std::sync::Arc::new(type_annotation_table);
    let expects_resolved = std::sync::Arc::new(expects_resolved);

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
    // Build type-stage environment (for builtin_eval_types). Falls back to stdlib_env if unavailable.
    let type_stage_env = tinct::build_type_stage_env().unwrap_or_else(|| Arc::clone(&env));

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
            .map_err(|_| "mtime is out of i64 range".to_string())?;
        let cap_value = Value::ClockCap(Rc::new(ClockCapInner::Fixed(nanos)));
        let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
        env.write()
            .unwrap()
            .insert("%clock".to_string(), Arc::new(cap_thunk));
    }

    // E3: Inject --cap-fs NAME=PATH[:MODE] entries (same as run_eval)
    {
        use tinct::Value;
        let cap_entries = open_cap_fs_entries(cap_fs, false)?;
        for (name, cap_dir_arc, perms) in cap_entries {
            // Clone the Arc to get an independent Rc for the DirCap value
            let dir_for_cap = Rc::new(cap_dir_arc.open_dir(".").expect("failed to dup cap dir"));
            let cap_value = Value::DirCap {
                dir: dir_for_cap,
                perms,
            };
            let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
            // Inject as `%NAME` (auto-prefix %).
            let scoped_name = if name.starts_with('%') {
                name.to_string()
            } else {
                format!("%{name}")
            };
            env.write()
                .unwrap()
                .insert(scoped_name, Arc::new(cap_thunk));
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
            env.write().unwrap().insert(name, Arc::new(cap_thunk));
        }
    }

    // Inject `%emit` channel into the root environment.
    // This is a bounded async channel with capacity 64 (same as eval-programs in loader.llt).
    // User code emits values via `[emit val]`, which sends to this channel.
    // For literate eval, the channel is created but never drained — emitted values are
    // discarded. This matches the semantics of `tinct run` without an output formatter:
    // the none.llt formatter drains %emit and discards all values.
    {
        use tinct::Value;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let channel_inner = tinct::ChannelInner {
            sender: tx,
            receiver: tokio::sync::Mutex::new(rx),
            capacity: 64,
        };
        let emit_value = Value::Channel(std::sync::Arc::new(channel_inner));
        let emit_thunk = tinct::Thunk::new_materialized(emit_value, tinct::Span::origin());
        env.write()
            .unwrap()
            .insert("%emit".to_string(), Arc::new(emit_thunk));
    }

    // Inject `%stdout` WriteHandle into the root environment.
    // Output formatters and user code can write directly to %stdout via [write-handle %stdout ...].
    {
        use std::cell::RefCell;
        use std::collections::HashMap;
        use std::io::BufWriter;
        use tinct::Value;

        // Create stdout WriteHandle with default caps (Bool(true) sentinel, consistent with stdin)
        let mut caps = HashMap::new();
        caps.insert("Writable".to_string(), Value::Bool(true));
        caps.insert("Text".to_string(), Value::Bool(true));

        let stdout_handle = Value::WriteHandle {
            caps,
            inner: Rc::new(RefCell::new(
                Box::new(BufWriter::new(std::io::stdout())) as Box<dyn std::io::Write>
            )),
        };
        let stdout_thunk = tinct::Thunk::new_materialized(stdout_handle, tinct::Span::origin());
        env.write()
            .unwrap()
            .insert("%stdout".to_string(), Arc::new(stdout_thunk));
    }

    // Literate mode always runs with --no-env (hard-coded, per doc comment).
    // env_allowed: Some(empty) = all env vars denied.
    let eval_ctx = EvalContext::new_with_options(
        base_dir,
        Arc::clone(&env),
        Arc::clone(&type_stage_env),
        false,
        false,
        Some(std::collections::HashSet::new()),
    );

    let thunk = tinct::async_rt::block_on(tinct::eval_surface_file_with_input(
        &program,
        Arc::clone(&env),
        &eval_ctx,
        &resolution_table,
        &type_annotation_table,
        &expects_resolved,
        None,
    ))
    .map_err(|e| {
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
    let json =
        visit_value(&val, &eval_ctx, 0, &JsonVisitor, thunk.definition_span()).map_err(|e| {
            let mut msg = format!("{e}");
            if let Some(snippet) = tinct::render_span_snippet(tangled, e.definition_span) {
                msg.push('\n');
                msg.push_str(&snippet);
            }
            msg
        })?;
    let output = tinct::json_pretty_print(&json);

    println!("{output}");

    Ok(())
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
fn run_literate_lint(tangled: &str, config: &LiterateConfig) -> Result<(), String> {
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

    // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> resolve -> typecheck.
    // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
    let lint_base_dir = open_file_base_dir(markdown_path, "literate lint")?;
    // Use expand_surface_program (not expand_macros) so SurfaceItem::Decl macros are seen.
    let mut program = output.program;
    tinct::async_rt::block_on_anywhere(tinct::expand::expand_surface_program(
        &mut program,
        false,
        &lint_base_dir,
    ))
    .map_err(|e| format!("{e}"))?;
    // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
    tinct::desugar::desugar_surface_program(&mut program);
    // Variable resolution pass (Phase 1 of arena allocation strategy).
    let _resolution_table = tinct::resolve::resolve_surface_program(&program);
    // Type check with prelude environment
    let env = tinct::build_prelude_env();
    let (type_errors, _type_map, _doc_map, _scheme_map, diagnostics) =
        tinct::typecheck::typecheck_surface_program(&program, env);

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
/// - Always runs with --no-cwd and --no-env (hard-coded)
/// - %clock is set to a fixed ClockCap from markdown file mtime
/// - %libdir is always available
/// - Capabilities injected via cap_fs and cap_net
// AMBIENT-OK: CLI literate-weave reading markdown file metadata for mtime.
#[allow(clippy::disallowed_methods)]
fn run_literate_weave(
    markdown: &str,
    blocks: &[String],
    config: &LiterateConfig,
) -> Result<(), String> {
    let markdown_path = config.file_path;
    let no_substitute = config.no_substitute;
    let strict = config.strict;
    let in_place = config.in_place;
    let verify = config.verify;
    let fail_on_errors = config.fail_on_errors;
    let cap_fs = config.cap_fs;
    let cap_net = config.cap_net;
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
    // Build type-stage environment (for builtin_eval_types). Falls back to stdlib_env if unavailable.
    let type_stage_env = tinct::build_type_stage_env().unwrap_or_else(|| Arc::clone(&env));

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
            .map_err(|_| "mtime is out of i64 range".to_string())?;
        let cap_value = Value::ClockCap(Rc::new(ClockCapInner::Fixed(nanos)));
        let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
        env.write()
            .unwrap()
            .insert("%clock".to_string(), Arc::new(cap_thunk));
    }

    // E3: Inject --cap-fs NAME=PATH[:MODE] entries (same as run_eval)
    {
        use tinct::Value;
        let cap_entries = open_cap_fs_entries(cap_fs, false)?;
        for (name, cap_dir_arc, perms) in cap_entries {
            // Clone the Arc to get an independent Rc for the DirCap value
            let dir_for_cap = Rc::new(cap_dir_arc.open_dir(".").expect("failed to dup cap dir"));
            let cap_value = Value::DirCap {
                dir: dir_for_cap,
                perms,
            };
            let cap_thunk = tinct::Thunk::new_materialized(cap_value, tinct::Span::origin());
            // Inject as `%NAME` (auto-prefix %).
            let scoped_name = if name.starts_with('%') {
                name.to_string()
            } else {
                format!("%{name}")
            };
            env.write()
                .unwrap()
                .insert(scoped_name, Arc::new(cap_thunk));
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
            env.write().unwrap().insert(name, Arc::new(cap_thunk));
        }
    }

    // Inject `%emit` channel into the root environment.
    // This is a bounded async channel with capacity 64 (same as eval-programs in loader.llt).
    // User code emits values via `[emit val]`, which sends to this channel.
    // For literate weave, the channel is created but never drained — emitted values are
    // discarded. This matches the semantics of `tinct run` without an output formatter:
    // the none.llt formatter drains %emit and discards all values.
    {
        use tinct::Value;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let channel_inner = tinct::ChannelInner {
            sender: tx,
            receiver: tokio::sync::Mutex::new(rx),
            capacity: 64,
        };
        let emit_value = Value::Channel(std::sync::Arc::new(channel_inner));
        let emit_thunk = tinct::Thunk::new_materialized(emit_value, tinct::Span::origin());
        env.write()
            .unwrap()
            .insert("%emit".to_string(), Arc::new(emit_thunk));
    }

    // Inject `%stdout` WriteHandle into the root environment.
    // Output formatters and user code can write directly to %stdout via [write-handle %stdout ...].
    {
        use std::cell::RefCell;
        use std::collections::HashMap;
        use std::io::BufWriter;
        use tinct::Value;

        // Create stdout WriteHandle with default caps (Bool(true) sentinel, consistent with stdin)
        let mut caps = HashMap::new();
        caps.insert("Writable".to_string(), Value::Bool(true));
        caps.insert("Text".to_string(), Value::Bool(true));

        let stdout_handle = Value::WriteHandle {
            caps,
            inner: Rc::new(RefCell::new(
                Box::new(BufWriter::new(std::io::stdout())) as Box<dyn std::io::Write>
            )),
        };
        let stdout_thunk = tinct::Thunk::new_materialized(stdout_handle, tinct::Span::origin());
        env.write()
            .unwrap()
            .insert("%stdout".to_string(), Arc::new(stdout_thunk));
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
        Arc::clone(&env),
        Arc::clone(&type_stage_env),
        false,
        false,
        Some(std::collections::HashSet::new()),
    );

    // Evaluate each block in turn, passing the previous result as pipeline input.
    // Collect (block_index -> actual output sections) for weaving/verification.
    let mut pipeline_input: Option<Arc<Thunk>> = None;

    // Split blocks into code + expectations
    let blocks_with_exp: Vec<_> = blocks
        .iter()
        .map(|s| literate::split_block_sections(s))
        .collect();

    let mut block_outputs: Vec<BlockOutput> = Vec::with_capacity(blocks_with_exp.len());

    for (i, block_with_exp) in blocks_with_exp.iter().enumerate() {
        let code = &block_with_exp.code;

        let parse_result = parse(code);
        let parsed = match parse_result {
            Ok(o) => o,
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

        // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> resolve -> typecheck.
        // Use expand_surface_program (not expand_macros) so SurfaceItem::Decl macros are seen.
        // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
        let mut program = parsed.program;
        match tinct::async_rt::block_on_anywhere(tinct::expand::expand_surface_program(
            &mut program,
            false,
            &base_eval_ctx.config.base_dir,
        )) {
            Ok(_) => {}
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
        }
        // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
        tinct::desugar::desugar_surface_program(&mut program);
        // Variable resolution pass (Phase 1 of arena allocation strategy).
        let resolution_table =
            std::sync::Arc::new(tinct::resolve::resolve_surface_program(&program));
        let (type_errors, type_annotation_table, expects_resolved) =
            tinct::typecheck::typecheck_surface_program_annotation_table(&program);
        let type_annotation_table = std::sync::Arc::new(type_annotation_table);

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

        let thunk_result = tinct::async_rt::block_on(tinct::eval_surface_file_with_input(
            &program,
            Arc::clone(&env),
            &eval_ctx,
            &resolution_table,
            &type_annotation_table,
            &expects_resolved,
            pipeline_input.clone(),
        ));
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
        let json = visit_value(&val, &eval_ctx, 0, &JsonVisitor, thunk.definition_span())
            .map_err(|e| format!("error serializing code block {} result: {e}", i + 1))?;
        let output_str = json;

        block_outputs.push(BlockOutput {
            out: Some(output_str),
            warn: type_warnings,
            error: None,
            info: None,
        });
        // Thread the result as pipeline input to the next block.
        pipeline_input = Some(Arc::clone(&thunk));
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

    // Inline marker substitution: replace <!-- tinct-result: EXPR --> markers
    // with the most-recent code block's result. When EXPR is empty the full JSON
    // is inserted; when EXPR is e.g. `%.x` the corresponding JSON field is
    // extracted.
    if !no_substitute {
        output = substitute_inline_markers(&output, &block_outputs);
    }

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
/// Markers with an expression (e.g., `<!-- tinct-result: %.x -->`) extract the
/// named field from the most recent block result. Markers without an expression
/// (just `<!-- tinct-result: -->`) are replaced with the full JSON output of the
/// most recent block.
///
/// The "most recent block" advances each time a tinct code block's closing fence
/// is encountered before the marker.
fn substitute_inline_markers(woven: &str, block_outputs: &[BlockOutput]) -> String {
    const MARKER_OPEN: &str = "<!-- tinct-result:";
    const MARKER_CLOSE: &str = "-->";

    let mut result = String::with_capacity(woven.len());
    let mut current_block: usize = 0;
    let mut in_tinct_block = false;

    for line in woven.lines() {
        let trimmed = line.trim();

        // Track code block boundaries to know which block result is "current"
        if !in_tinct_block && (trimmed == "```tinct" || trimmed == "```llt") {
            in_tinct_block = true;
        } else if in_tinct_block && trimmed == "```" {
            in_tinct_block = false;
            current_block += 1;
        }

        // Only substitute in non-code-block lines
        if !in_tinct_block && line.contains(MARKER_OPEN) {
            let mut line_result = String::with_capacity(line.len());
            let mut remaining = line;

            while let Some(open_pos) = remaining.find(MARKER_OPEN) {
                line_result.push_str(&remaining[..open_pos]);
                let after_open = &remaining[open_pos + MARKER_OPEN.len()..];

                if let Some(close_pos) = after_open.find(MARKER_CLOSE) {
                    let expr = after_open[..close_pos].trim();
                    let blk_idx = current_block.saturating_sub(1);

                    let replacement = if let Some(block) = block_outputs.get(blk_idx) {
                        if let Some(ref out) = block.out {
                            if expr.is_empty() {
                                out.clone()
                            } else {
                                resolve_inline_expr(expr, out)
                            }
                        } else {
                            // Block had an error — leave marker as-is
                            let marker_end =
                                open_pos + MARKER_OPEN.len() + close_pos + MARKER_CLOSE.len();
                            remaining[open_pos..marker_end].to_string()
                        }
                    } else {
                        // No block output — leave marker as-is
                        let marker_end =
                            open_pos + MARKER_OPEN.len() + close_pos + MARKER_CLOSE.len();
                        remaining[open_pos..marker_end].to_string()
                    };

                    line_result.push_str(&replacement);
                    remaining = &after_open[close_pos + MARKER_CLOSE.len()..];
                } else {
                    // No closing --> found, emit remainder as-is
                    line_result.push_str(&remaining[open_pos..]);
                    remaining = "";
                    break;
                }
            }

            line_result.push_str(remaining);
            result.push_str(&line_result);
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }

    result
}

/// Extract a top-level field value from a compact JSON object string.
///
/// Returns `Some(display_string)` if `json_output` is a JSON object containing `field`,
/// where the display string strips surrounding `"..."` for string values and returns
/// the raw fragment for numbers, booleans, and null.
///
/// Only handles the flat output of `JsonVisitor` (compact, no extra whitespace).
fn json_get_object_field(json_output: &str, field: &str) -> Option<String> {
    let s = json_output.trim();
    if !s.starts_with('{') || !s.ends_with('}') {
        return None;
    }
    // Build the key fragment to search for: `"<field>":`.
    // This handles the common case where keys do not themselves contain `"` or `\`.
    // For keys with special chars the escaped form would differ, but field names in
    // inline expressions are always simple identifiers.
    let needle = format!("\"{}\":", field);
    // Scan through the object manually to find the key.
    // We need string-aware scanning to skip over key/value strings safely.
    let inner = &s[1..s.len() - 1]; // strip outer { }
    let mut pos = 0;
    let bytes = inner.as_bytes();
    while pos < bytes.len() {
        // Skip whitespace
        while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t' | b'\n' | b'\r') {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        // We expect a quoted key
        if bytes[pos] != b'"' {
            break;
        }
        // Find end of key string (handle escapes)
        let key_start = pos;
        pos += 1; // skip opening "
        while pos < bytes.len() {
            if bytes[pos] == b'\\' {
                pos += 2; // skip escape sequence
            } else if bytes[pos] == b'"' {
                pos += 1; // skip closing "
                break;
            } else {
                pos += 1;
            }
        }
        // Skip ':'
        while pos < bytes.len() && bytes[pos] == b':' {
            pos += 1;
        }
        // Find end of value (handle strings, nested objects/arrays)
        let val_start = pos;
        if pos >= bytes.len() {
            break;
        }
        let val_end = json_scan_value(inner, pos);
        let key_fragment = &inner[key_start..];
        if key_fragment.starts_with(needle.as_str()) {
            // This is the key we want — extract the raw value
            let raw = inner[val_start..val_end].trim();
            // Convert to display string
            let display = if raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2 {
                // Single-pass unescape of a JSON string value.
                // Sequential .replace() chains are incorrect for inputs like \\n (escaped
                // backslash followed by n), which would be mishandled as a newline. A
                // single-pass character scanner handles all escape sequences correctly,
                // including \uXXXX Unicode escapes emitted by escape_json_str for control chars.
                let inner_str = &raw[1..raw.len() - 1];
                let mut result = String::with_capacity(inner_str.len());
                let mut chars = inner_str.chars();
                while let Some(c) = chars.next() {
                    if c == '\\' {
                        match chars.next() {
                            Some('"') => result.push('"'),
                            Some('\\') => result.push('\\'),
                            Some('n') => result.push('\n'),
                            Some('r') => result.push('\r'),
                            Some('t') => result.push('\t'),
                            Some('b') => result.push('\x08'),
                            Some('f') => result.push('\x0c'),
                            Some('/') => result.push('/'),
                            Some('u') => {
                                // Consume exactly 4 hex digits per RFC 8259 §7
                                let hex: String = chars.by_ref().take(4).collect();
                                if let Ok(n) = u32::from_str_radix(&hex, 16) {
                                    if let Some(ch) = char::from_u32(n) {
                                        result.push(ch);
                                    }
                                }
                                // If invalid, silently drop (malformed JSON input)
                            }
                            Some(c2) => {
                                result.push('\\');
                                result.push(c2);
                            }
                            None => {}
                        }
                    } else {
                        result.push(c);
                    }
                }
                result
            } else {
                raw.to_string()
            };
            return Some(display);
        }
        pos = val_end;
        // Skip ',' separator
        while pos < bytes.len() && matches!(bytes[pos], b',' | b' ' | b'\t' | b'\n' | b'\r') {
            pos += 1;
        }
    }
    None
}

/// Scan a JSON value starting at `pos` in `s`, returning the end position.
///
/// Handles strings (with escapes), arrays, objects, and primitives.
fn json_scan_value(s: &str, mut pos: usize) -> usize {
    let bytes = s.as_bytes();
    while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t') {
        pos += 1;
    }
    if pos >= bytes.len() {
        return pos;
    }
    match bytes[pos] {
        b'"' => {
            pos += 1;
            while pos < bytes.len() {
                if bytes[pos] == b'\\' {
                    pos += 2;
                } else if bytes[pos] == b'"' {
                    pos += 1;
                    break;
                } else {
                    pos += 1;
                }
            }
            pos
        }
        b'{' | b'[' => {
            let open = bytes[pos];
            let close = if open == b'{' { b'}' } else { b']' };
            let mut depth = 1usize;
            pos += 1;
            while pos < bytes.len() && depth > 0 {
                if bytes[pos] == b'"' {
                    pos += 1;
                    while pos < bytes.len() {
                        if bytes[pos] == b'\\' {
                            pos += 2;
                        } else if bytes[pos] == b'"' {
                            pos += 1;
                            break;
                        } else {
                            pos += 1;
                        }
                    }
                } else if bytes[pos] == open {
                    depth += 1;
                    pos += 1;
                } else if bytes[pos] == close {
                    depth -= 1;
                    pos += 1;
                } else {
                    pos += 1;
                }
            }
            pos
        }
        _ => {
            // Primitive: scan until ',' or '}' or ']' or end
            while pos < bytes.len() && !matches!(bytes[pos], b',' | b'}' | b']') {
                pos += 1;
            }
            pos
        }
    }
}

/// Resolve an inline expression against a JSON output string.
///
/// Supports `%.field` patterns (dot-key field extraction from a JSON object).
/// Falls back to the full output for unrecognized patterns.
fn resolve_inline_expr(expr: &str, json_output: &str) -> String {
    // Handle %.field pattern
    if let Some(field) = expr.strip_prefix("%.") {
        if let Some(display) = json_get_object_field(json_output, field) {
            return display;
        }
    }
    // Fallback: return full output
    json_output.to_string()
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

/// Simple JSON representation for the describe command.
/// This replaces serde_json usage in run_describe and its helpers.
#[derive(Debug, Clone)]
enum DescribeJson {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<DescribeJson>),
    Object(Vec<(String, DescribeJson)>),
}

impl DescribeJson {
    /// Convert to a pretty-printed JSON string with 2-space indentation.
    fn to_json_pretty(&self, indent_level: usize) -> String {
        let indent = "  ".repeat(indent_level);
        let next_indent = "  ".repeat(indent_level + 1);

        match self {
            DescribeJson::Bool(b) => b.to_string(),
            DescribeJson::Int(n) => n.to_string(),
            DescribeJson::Float(f) => {
                // Match serde_json behavior: finite floats only
                if f.is_finite() {
                    f.to_string()
                } else {
                    "null".to_string()
                }
            }
            DescribeJson::Str(s) => {
                // Delegate to the shared escape_json_str from lib.rs to avoid duplication.
                format!("\"{}\"", escape_json_str(s))
            }
            DescribeJson::Array(items) => {
                if items.is_empty() {
                    return "[]".to_string();
                }
                let mut result = "[\n".to_string();
                for (i, item) in items.iter().enumerate() {
                    result.push_str(&next_indent);
                    result.push_str(&item.to_json_pretty(indent_level + 1));
                    if i < items.len() - 1 {
                        result.push(',');
                    }
                    result.push('\n');
                }
                result.push_str(&indent);
                result.push(']');
                result
            }
            DescribeJson::Object(entries) => {
                if entries.is_empty() {
                    return "{}".to_string();
                }
                let mut result = "{\n".to_string();
                for (i, (key, value)) in entries.iter().enumerate() {
                    result.push_str(&next_indent);
                    // Key is always a string, so escape it
                    result.push_str(&DescribeJson::Str(key.clone()).to_json_pretty(0));
                    result.push_str(": ");
                    result.push_str(&value.to_json_pretty(indent_level + 1));
                    if i < entries.len() - 1 {
                        result.push(',');
                    }
                    result.push('\n');
                }
                result.push_str(&indent);
                result.push('}');
                result
            }
        }
    }

    /// Get the value as a string, if it's a string.
    fn as_str(&self) -> Option<&str> {
        match self {
            DescribeJson::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Get the value as an object (Vec of key-value pairs), if it's an object.
    fn as_object(&self) -> Option<&Vec<(String, DescribeJson)>> {
        match self {
            DescribeJson::Object(entries) => Some(entries),
            _ => None,
        }
    }

    /// Get a field from an object by key.
    fn get(&self, key: &str) -> Option<&DescribeJson> {
        match self {
            DescribeJson::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
}

/// Write content to a file atomically using a .tmp file then rename.
// AMBIENT-OK: CLI literate-weave --in-place writing to operator-specified file.
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
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

/// Describe the input contract of an LLT file.
///
/// Parses the file, extracts `%@Type` / `expects:` annotations from each document,
/// and detects schema dicts by heuristic. Outputs a human-readable summary (default)
/// or machine-readable JSON (`--json`).
// AMBIENT-OK: CLI describe — opens file parent dir for type-checking
#[allow(clippy::disallowed_methods)]
fn run_describe(file_path: &str, json_mode: bool) -> Result<(), String> {
    let sf = read_source(file_path)?;
    let source = String::from(&*sf.content);
    let output = parse_with_file(&source, Arc::clone(&sf)).map_err(|e| format!("{e}"))?;

    // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> resolve -> typecheck.
    // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
    // AMBIENT-OK: CLI bootstrap — operator specified this file path.
    let describe_base_dir = {
        let p = std::path::Path::new(file_path);
        let dir = p
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        cap_std::fs::Dir::open_ambient_dir(dir, cap_std::ambient_authority())
            .map_err(|e| format!("cannot open base directory for describe: {e}"))?
    };
    // Use expand_surface_program (not expand_macros) so SurfaceItem::Decl macros are seen.
    let mut program = output.program;
    tinct::async_rt::block_on_anywhere(tinct::expand::expand_surface_program(
        &mut program,
        false,
        &describe_base_dir,
    ))
    .map_err(|e| format!("{e}"))?;
    // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
    tinct::desugar::desugar_surface_program(&mut program);
    // Variable resolution pass (Phase 1 of arena allocation strategy).
    let _resolution_table = tinct::resolve::resolve_surface_program(&program);
    // Type check to get DocMap (for doc strings)
    let env = tinct::build_prelude_env();
    let (_type_errors, _type_map, doc_map, _scheme_map, _diagnostics) =
        tinct::typecheck::typecheck_surface_program(&program, env);

    // Collect contract information from each document section.
    let mut contracts: Vec<DescribeJson> = Vec::new();
    let mut has_any_contract = false;

    for (doc_idx, doc) in program.documents.iter().enumerate() {
        let mut doc_contract: Vec<(String, DescribeJson)> = Vec::new();
        doc_contract.push(("section".to_string(), DescribeJson::Int(doc_idx as i64)));

        // Extract expects: / %@Type annotation
        if let Some(ref ann) = doc.node.expects {
            has_any_contract = true;
            match &ann.node {
                tinct::Annotation::Simple(type_name) => {
                    doc_contract.push(("type".to_string(), DescribeJson::Str(type_name.clone())));
                }
                tinct::Annotation::PropertyDict(entries) => {
                    let mut fields: Vec<(String, DescribeJson)> = Vec::new();
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
                    if !fields.is_empty() {
                        doc_contract.push(("fields".to_string(), DescribeJson::Object(fields)));
                    }
                }
                tinct::Annotation::Annotated(name, _inner) => {
                    doc_contract.push(("type".to_string(), DescribeJson::Str(name.clone())));
                }
            }
        }

        // Detect schema dicts in the document expressions
        let schema_fields = detect_schema_dict(&doc.node);
        if !schema_fields.is_empty() {
            has_any_contract = true;
            doc_contract.push(("schema".to_string(), DescribeJson::Object(schema_fields)));
        }

        // Include doc strings from DocMap for top-level bindings
        let doc_strings = extract_doc_strings_from_doc(&doc.node, &doc_map);
        if !doc_strings.is_empty() {
            has_any_contract = true;
            doc_contract.push(("docs".to_string(), DescribeJson::Object(doc_strings)));
        }

        if doc_contract.len() > 1 {
            // Has more than just "section"
            contracts.push(DescribeJson::Object(doc_contract));
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
        let output = DescribeJson::Object(vec![(
            "contracts".to_string(),
            DescribeJson::Array(contracts),
        )]);
        let pretty = output.to_json_pretty(0);
        println!("{pretty}");
    } else {
        // Human-readable output: one line per field, with doc strings
        for contract in &contracts {
            if let Some(section) = contract.get("section") {
                if contracts.len() > 1 {
                    // Format the section number
                    let section_str = match section {
                        DescribeJson::Int(n) => n.to_string(),
                        _ => "?".to_string(),
                    };
                    println!("--- section {} ---", section_str);
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
                        if let Some(doc_str) = docs
                            .iter()
                            .find(|(k, _)| k == name)
                            .map(|(_, v)| v)
                            .and_then(|v| v.as_str())
                        {
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
                        if let Some(doc_str) = docs
                            .iter()
                            .find(|(k, _)| k == name)
                            .map(|(_, v)| v)
                            .and_then(|v| v.as_str())
                        {
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
                    .map(|o| o.iter().map(|(k, _)| k).collect())
                    .unwrap_or_default();
                let schema_names: std::collections::HashSet<&String> = contract
                    .get("schema")
                    .and_then(|s| s.as_object())
                    .map(|o| o.iter().map(|(k, _)| k).collect())
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
    doc: &tinct::ast::SurfaceDocument,
    doc_map: &std::collections::HashMap<String, String>,
) -> Vec<(String, DescribeJson)> {
    let mut result: Vec<(String, DescribeJson)> = Vec::new();

    for expr in doc.expressions() {
        if let tinct::ast::SurfaceExpression::Dict(entries) = &expr.expr {
            for entry in entries {
                if let Some(ref key_node) = entry.node.key {
                    // Extract the binding name from the key expression
                    // Keys can be:
                    // - SurfaceExpression::Str (string literal key)
                    // - SurfaceExpression::Annotated { name, .. } (annotated binding like name@[...])
                    // - SurfaceExpression::VarRef (bare identifier key)
                    let name_opt = match &key_node.expr {
                        tinct::ast::SurfaceExpression::Str(s) => Some(s.as_str()),
                        tinct::ast::SurfaceExpression::Annotated { name, .. } => {
                            Some(name.as_str())
                        }
                        tinct::ast::SurfaceExpression::VarRef { name, .. } => Some(name.as_str()),
                        _ => None,
                    };

                    if let Some(name) = name_opt {
                        if let Some(doc_str) = doc_map.get(name) {
                            result.push((name.to_string(), DescribeJson::Str(doc_str.clone())));
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
fn detect_schema_dict(doc: &tinct::ast::SurfaceDocument) -> Vec<(String, DescribeJson)> {
    let mut result: Vec<(String, DescribeJson)> = Vec::new();
    for expr in doc.expressions() {
        if let tinct::ast::SurfaceExpression::Dict(entries) = &expr.expr {
            for entry in entries {
                if let Some(ref key_node) = entry.node.key {
                    if let tinct::ast::SurfaceExpression::Str(ref field_name) = key_node.expr {
                        // Check if the value is a dict with schema keys
                        if let Some(schema_info) = extract_schema_info(&entry.node.value.expr) {
                            result.push((field_name.clone(), schema_info));
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
fn extract_schema_info(expr: &tinct::ast::SurfaceExpression) -> Option<DescribeJson> {
    if let tinct::ast::SurfaceExpression::Dict(entries) = expr {
        let mut info: Vec<(String, DescribeJson)> = Vec::new();
        let mut has_schema_key = false;
        for entry in entries {
            if let Some(ref key_node) = entry.node.key {
                if let tinct::ast::SurfaceExpression::Str(ref key_name) = key_node.expr {
                    if SCHEMA_KEYS.contains(&key_name.as_str()) {
                        has_schema_key = true;
                        info.push((
                            key_name.clone(),
                            describe_surface_annotation_value(&entry.node.value.expr),
                        ));
                    }
                }
            }
        }
        if has_schema_key {
            return Some(DescribeJson::Object(info));
        }
    }
    None
}

/// Turn a surface annotation value expression into a JSON description.
fn describe_surface_annotation_value(expr: &tinct::ast::SurfaceExpression) -> DescribeJson {
    match expr {
        tinct::ast::SurfaceExpression::Str(s) => DescribeJson::Str(s.clone()),
        tinct::ast::SurfaceExpression::Int(n) => DescribeJson::Int(*n),
        tinct::ast::SurfaceExpression::Float(f) => DescribeJson::Float(*f),
        tinct::ast::SurfaceExpression::Bool(b) => DescribeJson::Bool(*b),
        tinct::ast::SurfaceExpression::VarRef { name, .. } => DescribeJson::Str(name.clone()),
        _ => DescribeJson::Str("(complex)".to_string()),
    }
}

/// Format a constraint JSON value as a human-readable string.
fn format_constraint(val: &DescribeJson) -> String {
    match val {
        DescribeJson::Str(s) => s.clone(),
        DescribeJson::Object(entries) => {
            let parts: Vec<String> = entries
                .iter()
                .map(|(k, v)| {
                    let v_str = match v {
                        DescribeJson::Str(s) => s.clone(),
                        DescribeJson::Int(n) => n.to_string(),
                        DescribeJson::Float(f) => f.to_string(),
                        DescribeJson::Bool(b) => b.to_string(),
                        DescribeJson::Array(_) => "[...]".to_string(),
                        DescribeJson::Object(_) => "{...}".to_string(),
                    };
                    format!("{k}: {v_str}")
                })
                .collect();
            parts.join(", ")
        }
        DescribeJson::Int(n) => n.to_string(),
        DescribeJson::Float(f) => f.to_string(),
        DescribeJson::Bool(b) => b.to_string(),
        DescribeJson::Array(_) => "[...]".to_string(),
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

    // -------------------------------------------------------------------------
    // json_get_object_field tests
    // -------------------------------------------------------------------------

    #[test]
    fn json_get_object_field_string_value() {
        assert_eq!(
            json_get_object_field(r#"{"host":"localhost"}"#, "host"),
            Some("localhost".to_string())
        );
    }

    #[test]
    fn json_get_object_field_number_value() {
        assert_eq!(
            json_get_object_field(r#"{"port":8080}"#, "port"),
            Some("8080".to_string())
        );
    }

    #[test]
    fn json_get_object_field_missing_key() {
        assert_eq!(json_get_object_field(r#"{"x":1}"#, "y"), None);
    }

    #[test]
    fn json_get_object_field_not_an_object() {
        assert_eq!(json_get_object_field(r#"[1,2,3]"#, "x"), None);
    }

    #[test]
    fn json_get_object_field_escaped_backslash_n_in_value() {
        // JSON: {"k":"a\\nb"} — the value is escaped backslash + n (not newline).
        // After unescaping: a\nb (backslash + n as two characters).
        assert_eq!(
            json_get_object_field(r#"{"k":"a\\nb"}"#, "k"),
            Some("a\\nb".to_string())
        );
    }

    #[test]
    fn json_get_object_field_unicode_escape() {
        // JSON escape sequence decodes to U+0001 (SOH control character).
        assert_eq!(
            json_get_object_field("{\"k\":\"\\u0001\"}", "k"),
            Some("\x01".to_string())
        );
    }
}
