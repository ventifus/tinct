//! LLT command-line tool: parses and evaluates `.llt` files, outputs JSON or LLT display format.

use clap::{Parser, Subcommand, ValueEnum};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, IsTerminal, Read};
use std::process;
use std::rc::Rc;
use tinct::{
    clear_include_context, create_stdlib_env, deep_materialize, eval_file_with_input,
    format_source, json_to_value, materialize, parse, set_include_context, value_to_display_string,
    value_to_json, IncludeContext, Span, Thunk, MAX_FILE_SIZE,
};

const WORKER_STACK_SIZE: usize = 64 * 1024 * 1024;

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

    let result = std::thread::Builder::new()
        .stack_size(WORKER_STACK_SIZE)
        .spawn(move || match cli.command {
            Commands::Eval { format, eval, file } => run_eval(&file, &format, eval),
            Commands::Fmt {
                check,
                in_place,
                file,
            } => run_fmt(&file, check, in_place),
            #[cfg(feature = "repl")]
            Commands::Repl => tinct::repl::run_repl(),
            #[cfg(feature = "lsp")]
            Commands::Lsp => tinct::lsp::run_lsp().map_err(|e| format!("{e}")),
        })
        .expect("failed to spawn worker thread")
        .join();

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("{e}");
            process::exit(1);
        }
        Err(_) => {
            eprintln!("internal error: worker thread panicked");
            process::exit(2);
        }
    }
}

fn run_eval(file_path: &str, format: &OutputFormat, force_eval: bool) -> Result<(), String> {
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

    // Desugar $_ implicit lambdas (pre-typecheck AST transformation)
    tinct::desugar::desugar_file(&mut ast.node);

    // Create stdlib environment
    let env = create_stdlib_env().map_err(|e| format!("{e}"))?;

    // Set up include context for $include builtin
    let base_dir = if file_path == "-" {
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
    set_include_context(IncludeContext {
        base_dir,
        include_guard: Rc::new(RefCell::new(HashSet::new())),
        stdlib_env: Rc::clone(&env),
        cache: Rc::new(RefCell::new(HashMap::new())),
    });

    // Wrap evaluation logic in a closure so that clear_include_context() runs
    // on all exit paths (success and error), preventing stale thread-local state.
    let result = (|| {
        let initial_input = stdin_input;

        // Evaluate
        let thunk =
            eval_file_with_input(&ast.node, env, initial_input, 0).map_err(|e| format!("{e}"))?;

        // Materialize the result
        let val = materialize(&thunk, None, 0).map_err(|e| format!("{e}"))?;

        // Optionally deep-force all thunks
        let val = if force_eval {
            deep_materialize(&val, 0).map_err(|e| format!("{e}"))?
        } else {
            val
        };

        // Serialize and output
        match format {
            OutputFormat::Json => {
                let json = value_to_json(&val, 0).map_err(|e| format!("{e}"))?;
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
                    &deep_materialize(&val, 0).map_err(|e| format!("{e}"))?
                };
                let output = value_to_display_string(display_val, 0).map_err(|e| format!("{e}"))?;
                println!("{output}");
            }
        }

        Ok(())
    })();

    clear_include_context();
    result
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
        let metadata =
            std::fs::metadata(file_path).map_err(|e| format!("error reading file: {e}"))?;
        if metadata.len() > MAX_FILE_SIZE {
            return Err(format!(
                "input file is {} bytes, which exceeds the 10 MB limit ({} bytes)",
                metadata.len(),
                MAX_FILE_SIZE
            ));
        }
        std::fs::read_to_string(file_path).map_err(|e| format!("error reading file: {e}"))
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
