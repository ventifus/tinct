# tinct - Containerized Build System
# All commands run in containers, no local Rust installation required

# Container runtime (podman or docker)
container := "podman"

# Rust version to use
rust_version := "1.95"

# Nightly image — required for cargo-fuzz / libfuzzer instrumentation
nightly_image := "rust:nightly"

# Container image
rust_image := "rust:" + rust_version

# Project name for volume naming
project_name := "tinct"

# Common container run flags
# target/ is a bind mount so binaries land on the host (symlinkable from ~/.local/bin)
# cargo registry cache stays a named volume — no need to expose it on the host
run_flags := "--rm --memory 8g -v .:/workspace:z -v ./target:/workspace/target:z -v " + project_name + "-cargo:/usr/local/cargo/registry -w /workspace"

# Default recipe - show available commands
default:
    @just --list

# Build the project (debug mode)
build:
    {{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} cargo build

# Install release binary to ~/.local/bin/tinct (symlink)
install: build-release
    mkdir -p ~/.local/bin
    ln -sf "$(pwd)/target/release/tinct" ~/.local/bin/tinct
    @echo "Symlinked target/release/tinct → ~/.local/bin/tinct"

# Build the project (release mode)
build-release:
    {{container}} run {{run_flags}} {{rust_image}} cargo build --release

# Run all tests: lib tests (single-threaded to prevent parallel 128MB-thread exhaustion)
# followed by corpus integration tests, CLI integration tests, and LSP corpus tests, in separate containers.
# --test-threads=1 serializes deep-eval tests (each 128MB unnamed thread) so only one runs at a time.
test:
    {{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --lib -- --test-threads=1
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --test corpus_tests -- --test-threads=1
    {{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --test cli_tests
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --features lsp --test lsp_corpus_tests -- --test-threads=1

# Run tests with output (NOTE: LSP corpus tests require `just test-lsp` — they need --features lsp)
test-verbose:
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 {{rust_image}} cargo test -- --nocapture

# Run a specific test
test-one TEST:
    {{container}} run {{run_flags}} {{rust_image}} cargo test {{TEST}}

# Run only lib unit tests (no integration tests)
test-lib:
    {{container}} run {{run_flags}} {{rust_image}} cargo test --lib

# Run only corpus tests (NOTE: does not include LSP corpus tests — use `just test-lsp` for those)
test-corpus:
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 {{rust_image}} cargo test --test corpus_tests

# Run only LSP corpus tests (requires --features lsp)
test-lsp:
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 {{rust_image}} cargo test --features lsp --test lsp_corpus_tests -- --test-threads=1

# Run clippy (linter)
lint:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rustup component add clippy 2>/dev/null; cargo clippy -- -D warnings"

# Run clippy with auto-fixes
lint-fix:
    {{container}} run {{run_flags}} {{rust_image}} cargo clippy --fix --allow-dirty --allow-staged

# Check code formatting
fmt-check:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rustup component add rustfmt 2>/dev/null; cargo fmt -- --check"

# Format code
fmt:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rustup component add rustfmt 2>/dev/null; cargo fmt"

# Run the application with samples/basic.llt (eval, JSON output)
run:
    {{container}} run {{run_flags}} {{rust_image}} cargo run --bin tinct -- run samples/basic.llt

# Run the application with custom input file
run-file FILE:
    {{container}} run {{run_flags}} {{rust_image}} cargo run --bin tinct -- run {{FILE}}

# Run with LLT display format
run-llt FILE:
    {{container}} run {{run_flags}} {{rust_image}} cargo run --bin tinct -- run -o llt {{FILE}}

# Run with piped JSON stdin
run-json JSON FILE:
    echo '{{JSON}}' | {{container}} run -i {{run_flags}} {{rust_image}} cargo run --bin tinct -- run {{FILE}}

# Run the release build
run-release:
    {{container}} run {{run_flags}} {{rust_image}} cargo run --bin tinct --release -- run samples/basic.llt

# Clean build artifacts
clean:
    {{container}} run {{run_flags}} {{rust_image}} cargo clean

# Check if the code compiles without building
check:
    {{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} cargo check

# Update dependencies
update:
    {{container}} run {{run_flags}} {{rust_image}} cargo update

# Pin a specific dependency version
update-precise PKG VER:
    {{container}} run {{run_flags}} {{rust_image}} cargo update {{PKG}} --precise {{VER}}

# Show dependency tree
tree:
    {{container}} run {{run_flags}} {{rust_image}} cargo tree

# Run full CI pipeline (check, test, lint, fmt-check, audit)
ci:
    {{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} cargo check
    just test
    just lint
    just fmt-check
    just audit
    @echo "✅ All CI checks passed!"

# Start interactive REPL
repl:
    {{container}} run -it {{run_flags}} {{rust_image}} cargo run --bin tinct -- repl

# Start LSP server (stdio transport)
lsp:
    {{container}} run -i {{run_flags}} {{rust_image}} cargo run --bin tinct -- lsp

# Tree-sitter grammar
node_image := "node:22"
ts_run_flags := "--rm -v .:/workspace:z -w /workspace/tree-sitter-llt"

# Generate tree-sitter parser from grammar.js
ts-generate:
    {{container}} run {{ts_run_flags}} -v {{project_name}}-ts-node:/workspace/tree-sitter-llt/node_modules {{node_image}} sh -c "npm install --no-save tree-sitter-cli && npx tree-sitter generate"

# Run tree-sitter tests
ts-test:
    {{container}} run {{ts_run_flags}} -v {{project_name}}-ts-node:/workspace/tree-sitter-llt/node_modules {{node_image}} sh -c "npm install --no-save tree-sitter-cli && npx tree-sitter test"

# Parse a file with tree-sitter
ts-parse FILE:
    {{container}} run {{ts_run_flags}} -v {{project_name}}-ts-node:/workspace/tree-sitter-llt/node_modules {{node_image}} sh -c "npm install --no-save tree-sitter-cli && npx tree-sitter parse /workspace/{{FILE}}"

# Build and package the VS Code extension as a .vsix file
ext:
    {{container}} run --rm -v .:/workspace:z -w /workspace/editors/vscode {{node_image}} sh -c "npm install && npm run compile && npx @vscode/vsce package --no-dependencies"

# Format LLT source file and print to stdout
fmt-llt FILE:
    {{container}} run {{run_flags}} {{rust_image}} cargo run --bin tinct -- fmt {{FILE}}

# Check LLT source formatting (exit 1 if unformatted)
fmt-llt-check FILE:
    {{container}} run {{run_flags}} {{rust_image}} cargo run --bin tinct -- fmt --check {{FILE}}

# Format LLT source file in place
fmt-llt-fix FILE:
    {{container}} run {{run_flags}} {{rust_image}} cargo run --bin tinct -- fmt --in-place {{FILE}}

# Show Rust version
version:
    {{container}} run {{run_flags}} {{rust_image}} rustc --version
    {{container}} run {{run_flags}} {{rust_image}} cargo --version

# Show current vs latest versions of Rust toolchain and all direct dependencies.
# Reads Cargo.lock for locked versions; queries crates.io and rust-lang.org via HTTPS.
# Runs on host (no container overhead) — requires tinct in PATH (just install).
versions:
    RUST_VERSION={{rust_version}} tinct run --cap-net nc=static.rust-lang.org:443 --cap-net nc=crates.io:443 samples/versions.llt

# Build documentation
doc:
    {{container}} run {{run_flags}} {{rust_image}} cargo doc --no-deps

# Build and open documentation
doc-open:
    {{container}} run {{run_flags}} {{rust_image}} cargo doc --no-deps --open

# Generate stdlib reference documentation from annotated source (stdlib/prelude.llt).
# TODO: implement a real generator that reads @doc annotations and produces doc/11-stdlib.md entries.
# Currently a stub that confirms the annotated source is present.
docs:
    @echo "stdlib reference docs: not yet implemented (stub)"
    @test -f stdlib/prelude.llt && echo "stdlib/prelude.llt present ($(wc -l < stdlib/prelude.llt) lines)" || (echo "ERROR: stdlib/prelude.llt missing" && exit 1)

# Run cargo bench (if benchmarks exist)
bench:
    {{container}} run {{run_flags}} {{rust_image}} cargo bench

# Generate LLVM coverage report (requires cargo-llvm-cov; install with: cargo install cargo-llvm-cov)
# Opens the HTML report in the default browser after generation.
coverage:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rustup component add llvm-tools-preview 2>/dev/null; cargo llvm-cov --open"

# Property-based testing via proptest (planned — see doc/whatif/eval-semantics-verification.md §Part A)
proptest:
    @echo "proptest: add proptest = \"1\" to [dev-dependencies] in Cargo.toml, then run: just test"

# ---------------------------------------------------------------------------
# Fuzz Testing (cargo-fuzz, requires nightly Rust)
# Targets: parse  eval_source  typecheck_source
# Corpus and crash artifacts land in fuzz/corpus/ and fuzz/artifacts/ (gitignored).
# ---------------------------------------------------------------------------

# Run a named fuzz target for TIME seconds (default 60s).
# Example: just fuzz parse 300
fuzz TARGET TIME="60":
    {{container}} run {{run_flags}} {{nightly_image}} sh -c "cargo install cargo-fuzz --locked && cargo fuzz run {{TARGET}} -- -max_total_time={{TIME}}"

# Quick smoke run: each target for 30 seconds — catches immediate panics
fuzz-smoke:
    just fuzz parse 30
    just fuzz eval_source 30
    just fuzz typecheck_source 30

# Build all fuzz targets without running (compile check on nightly)
fuzz-build:
    {{container}} run {{run_flags}} {{nightly_image}} sh -c "cargo install cargo-fuzz --locked && cargo fuzz build"

# List available fuzz targets
fuzz-list:
    {{container}} run {{run_flags}} {{nightly_image}} sh -c "cargo install cargo-fuzz --locked && cargo fuzz list"

# Generate stdlib reference docs from annotations
# TODO: implement — parse doc comments from stdlib/prelude.llt and src/builtins.rs,
#   merge by category, emit to doc/11-stdlib-reference.md
stdlib-docs:
    @echo "TODO: stdlib doc generation not yet implemented."
    @echo "Planned: parse ## comments from stdlib/prelude.llt + /// from src/builtins.rs"
    @echo "Output target: doc/11-stdlib-reference.md"

# Audit dependencies for security vulnerabilities
audit:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "cargo install cargo-audit@0.22.1 --locked && cargo audit"

# Watch for changes and run tests (requires cargo-watch)
watch:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "cargo install cargo-watch && cargo watch -x test"

# Remove all container images (cleanup)
clean-images:
    {{container}} rmi {{rust_image}} || true

# Remove build volumes (WARNING: clears all build cache)
clean-volumes:
    rm -rf ./target
    {{container}} volume rm {{project_name}}-cargo || true

# Full cleanup (images + volumes)
clean-all: clean-images clean-volumes
    @echo "✅ All containers and volumes removed"

# Show volume disk usage
volume-info:
    @echo "Build artifact volumes:"
    @{{container}} volume inspect {{project_name}}-target {{project_name}}-cargo 2>/dev/null || echo "Volumes not yet created"
    @echo ""
    @echo "Volume disk usage:"
    @{{container}} system df -v 2>/dev/null | grep -E "(VOLUME NAME|{{project_name}})" || true

# Download RFC reference documents to .training/rfcs/
download-rfcs:
    mkdir -p .training/rfcs
    curl -sSL https://www.rfc-editor.org/rfc/rfc3986.txt -o .training/rfcs/rfc3986.txt
    curl -sSL https://www.rfc-editor.org/rfc/rfc7320.txt -o .training/rfcs/rfc7320.txt
    curl -sSL https://www.rfc-editor.org/rfc/rfc8820.txt -o .training/rfcs/rfc8820.txt
    curl -sSL https://www.rfc-editor.org/rfc/rfc8141.txt -o .training/rfcs/rfc8141.txt
    curl -sSL https://www.rfc-editor.org/rfc/rfc3987.txt -o .training/rfcs/rfc3987.txt
    @echo "Downloaded RFCs to .training/rfcs/"

# Add === warn sections to corpus files that produce type warnings
add-warn-sections:
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 {{rust_image}} cargo run --example add_warn_sections

# Clean up stale === warn sections in corpus test files
cleanup-warn-sections:
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 {{rust_image}} cargo run --example cleanup_warn_sections

# Migrate === out to === error in eval/errors/ corpus test files
migrate-error-sections:
    {{container}} run {{run_flags}} {{rust_image}} cargo run --example migrate_error_sections

# Add === warn sections to error files that produce type warnings
add-warn-to-error-files:
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 {{rust_image}} cargo run --example add_warn_to_error_files
