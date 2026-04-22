use tinct::formatter::format_source;

fn main() {
    let test_cases = vec![
        ("[]", "[]\n"),
        ("[x: 1]", "[x: 1]\n"),
        ("[x: 1; y: 2]", "[x: 1 y: 2]\n"),
        ("[a: 1 b: 2 c: 3 d: 4]", "[a: 1 b: 2 c: 3 d: 4]\n"),
        ("[1 2 3]", "[1 2 3]\n"),
    ];

    let mut passed = 0;
    let mut failed = 0;

    for (input, expected) in test_cases {
        match format_source(input) {
            Ok(result) => {
                if result == expected {
                    println!("✓ {:?}", input);
                    passed += 1;
                } else {
                    println!("✗ {:?}", input);
                    println!("  Expected: {:?}", expected);
                    println!("  Got:      {:?}", result);
                    failed += 1;
                }
            }
            Err(e) => {
                println!("✗ {:?} - Error: {}", input, e);
                failed += 1;
            }
        }
    }

    println!("\n{} passed, {} failed", passed, failed);
    if failed > 0 {
        std::process::exit(1);
    }
}
