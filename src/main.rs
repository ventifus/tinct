//! LLT command-line tool: parses and evaluates `.llt` files, outputs JSON or LLT display format.

use clap::{Parser, Subcommand, ValueEnum};
use std::io::{self, IsTerminal, Read};
use std::process;
use std::rc::Rc;
use tinct::{
    create_stdlib_env, deep_materialize, eval_file_with_input, format_source, json_to_value,
    materialize, parse, value_to_display_string, value_to_json, Span, Thunk, MAX_FILE_SIZE,
};

// Exit codes for llt eval
const EXIT_ERROR: i32 = 1;
const EXIT_TIMEOUT: i32 = 2;
// EXIT_RESOURCE: i32 = 3; // reserved for future --max-memory/--max-cpu

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

        /// Wall-clock timeout (e.g. "5s", "500ms", "2m"). Exit code 2 on expiry.
        #[arg(long)]
        timeout: Option<String>,

        /// Input LLT file. Use `-` to read LLT source from stdin.
        file: String,
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
    /// Start an interactive REPL session.
    #[cfg(feature = "repl")]
    Repl,
    /// Start the LSP server (stdio transport).
    #[cfg(feature = "lsp")]
    Lsp,
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
            timeout,
            file,
        } => run_eval(&file, &format, eval, no_fs, timeout.as_deref()),
        Commands::Fmt {
            check,
            in_place,
            file,
        } => run_fmt(&file, check, in_place),
        #[cfg(feature = "repl")]
        Commands::Repl => tinct::repl::run_repl(),
        #[cfg(feature = "lsp")]
        Commands::Lsp => tinct::lsp::run_lsp().map_err(|e| format!("{e}")),
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

fn run_eval(
    file_path: &str,
    format: &OutputFormat,
    force_eval: bool,
    no_fs: bool,
    timeout: Option<&str>,
) -> Result<(), String> {
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
    // Read the LLT source
    let source = read_source(file_path)?;

    // Check for piped stdin JSON (only when file is not stdin itself)
    let stdin_input = if file_path != "-" {
        read_stdin_json()?
    } else {
        None
    };

    // Parse
    let mut ast = parse(&source).map_err(|e| format!("{e}"))?;

    // PIPELINE INVARIANT: Desugar must run after parse and before typecheck.
    // See also: src/lib.rs:87-91 (eval_source_with_config pipeline)
    // Desugar $_ implicit lambdas (mandatory pre-typecheck AST transformation).
    tinct::desugar::desugar_file(&mut ast.node);

    // Type errors are advisory; evaluation proceeds regardless.
    let _ = tinct::typecheck::typecheck_file(&ast.node);

    // Create stdlib environment
    let env = create_stdlib_env().map_err(|e| format!("{e}"))?;

    // Determine base directory for $include resolution
    let base_dir_path = if file_path == "-" {
        std::env::current_dir().map_err(|e| format!("cannot determine working directory: {e}"))?
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
    let base_dir = cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
        .map_err(|e| format!("cannot open base directory: {e}"))?;

    // Create evaluation context (includes base_dir, stdlib_env, include_guard, include_cache)
    let eval_ctx = tinct::EvalContext::new(base_dir, Rc::clone(&env), no_fs);

    let initial_input = stdin_input;

    // Evaluate
    let thunk = eval_file_with_input(&ast.node, Rc::clone(&env), &eval_ctx, initial_input, 0)
        .map_err(|e| format!("{e}"))?;

    // Materialize the result
    let val = materialize(&thunk, None, &eval_ctx, 0).map_err(|e| format!("{e}"))?;

    // Optionally deep-force all thunks
    let val = if force_eval {
        deep_materialize(&val, &eval_ctx, 0, None).map_err(|e| format!("{e}"))?
    } else {
        val
    };

    // Serialize and output
    match format {
        OutputFormat::Json => {
            let json = value_to_json(&val, &eval_ctx, 0).map_err(|e| format!("{e}"))?;
            let output = serde_json::to_string_pretty(&json)
                .map_err(|e| format!("JSON serialization error: {e}"))?;
            println!("{output}");
        }
        OutputFormat::Llt => {
            // Deep-materialize for display (value_to_display_string needs it).
            // Skip if --eval already deep-materialized above.
            let display_val = if force_eval {
                &val
            } else {
                &deep_materialize(&val, &eval_ctx, 0, None).map_err(|e| format!("{e}"))?
            };
            let output =
                value_to_display_string(display_val, &eval_ctx, 0).map_err(|e| format!("{e}"))?;
            println!("{output}");
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
/// to an LLT Value for injection as `$$` in the first document.
fn read_stdin_json() -> Result<Option<Rc<Thunk>>, String> {
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

    let val = json_to_value(&json, 0, Span::origin()).map_err(|e| format!("{e}"))?;
    Ok(Some(val))
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
