use lazy_lisp_transformer::parse;
use std::env;
use std::fs;
use std::process;

/// Maximum input file size: 10 MB.
const MAX_INPUT_SIZE: u64 = 10 * 1024 * 1024;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: {} <input_file>", args[0]);
        process::exit(1);
    }

    let file_size = match fs::metadata(&args[1]) {
        Ok(m) => m.len(),
        Err(e) => {
            eprintln!("error: reading file metadata: {e}");
            process::exit(1);
        }
    };

    if file_size > MAX_INPUT_SIZE {
        eprintln!(
            "error: input file is {} bytes, which exceeds the 10 MB limit ({} bytes)",
            file_size, MAX_INPUT_SIZE
        );
        process::exit(1);
    }

    let content = match fs::read_to_string(&args[1]) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: reading file: {e}");
            process::exit(1);
        }
    };

    match parse(&content) {
        Ok(ast) => println!("{:#?}", ast),
        Err(e) => {
            eprintln!("{e}");
            process::exit(1);
        }
    }
}
