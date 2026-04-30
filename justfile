# tinct - Containerized Build System
# All commands run in containers, no local Rust installation required

# Container runtime (podman or docker)
container := "podman"

# Rust version to use
rust_version := "1.86"

# Container image
rust_image := "rust:" + rust_version

# Project name for volume naming
project_name := "tinct"

# Common container run flags (using named volumes for target and cargo cache)
run_flags := "--rm -v .:/workspace:z -v " + project_name + "-target:/workspace/target -v " + project_name + "-cargo:/usr/local/cargo/registry -w /workspace"

# User flag to match host UID/GID (prevents permission issues)
user_flag := "--user $(id -u):$(id -g)"

# Default recipe - show available commands
default:
    @just --list

# Build the project (debug mode)
build:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo build

# Build the project (release mode)
build-release:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo build --release

# Run all tests: lib tests (single-threaded to prevent parallel 128MB-thread exhaustion)
# followed by corpus integration tests, in separate containers.
# --test-threads=1 serializes deep-eval tests (each 128MB unnamed thread) so only one runs at a time.
test:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo test --lib -- --test-threads=1
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo test --test corpus_tests

# Run tests with output
test-verbose:
    {{container}} run {{run_flags}} {{user_flag}} -e RUST_MIN_STACK=67108864 {{rust_image}} cargo test -- --nocapture

# Run a specific test
test-one TEST:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo test {{TEST}}

# Run only lib unit tests (no integration tests)
test-lib:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo test --lib

# Run only corpus tests
test-corpus:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo test --test corpus_tests

# Run clippy (linter)
lint:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} sh -c "rustup component add clippy 2>/dev/null; cargo clippy -- -D warnings"

# Run clippy with auto-fixes (runs as container root so it can write to bind-mounted source files)
lint-fix:
    {{container}} run {{run_flags}} {{rust_image}} cargo clippy --fix --allow-dirty --allow-staged

# Check code formatting
fmt-check:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} sh -c "rustup component add rustfmt 2>/dev/null; cargo fmt -- --check"

# Format code (runs as container root so it can write to bind-mounted source files)
fmt:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rustup component add rustfmt 2>/dev/null; cargo fmt"

# Run the application with test_input.llt (eval, JSON output)
run:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin tinct -- eval test_input.llt

# Run the application with custom input file
run-file FILE:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin tinct -- eval {{FILE}}

# Run with LLT display format
run-llt FILE:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin tinct -- eval -f llt {{FILE}}

# Run with piped JSON stdin
run-json JSON FILE:
    echo '{{JSON}}' | {{container}} run -i {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin tinct -- eval {{FILE}}

# Run the release build
run-release:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin tinct --release -- eval test_input.llt

# Clean build artifacts
clean:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo clean

# Clean build artifacts as root (use if permission errors occur)
clean-root:
    {{container}} run {{run_flags}} {{rust_image}} cargo clean

# Check if the code compiles without building
check:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo check

# Update dependencies
update:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo update

# Update dependencies as root (use if permission errors occur).
# Runs without {{user_flag}} because the cargo cache volume may lack write access
# for the host user inside the container; root always has write access.
update-root:
    {{container}} run {{run_flags}} {{rust_image}} cargo update

# Downgrade dependencies that require Rust 1.87+; pin to last Rust-1.86-compatible versions.
# Pinned crates and reasons:
#   home 0.5.5          — newer versions require Rust 1.87+
#   url 2.5.3           — newer versions depend on idna ≥ 1.0 which requires Rust 1.87+
#   idna_adapter 1.2.0  — newer versions (via idna 1.x) require Rust 1.87+
# (icu_* packages are pulled in transitively through idna; pinning idna_adapter is sufficient)
# Runs as root for the same reason as update-root (cargo cache volume permissions).
downgrade-deps:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "cargo update home --precise 0.5.5 && cargo update url --precise 2.5.3 && cargo update idna_adapter --precise 1.2.0"

# Pin a specific dependency version
update-precise PKG VER:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo update {{PKG}} --precise {{VER}}

# Show dependency tree
tree:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo tree

# Run full CI pipeline (check, test, lint, fmt-check)
ci: check test lint fmt-check
    @echo "✅ All CI checks passed!"

# Start interactive REPL
repl:
    {{container}} run -it {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin tinct -- repl

# Start LSP server (stdio transport)
lsp:
    {{container}} run -i {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin tinct -- lsp

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

# Format LLT source file and print to stdout
fmt-llt FILE:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin tinct -- fmt {{FILE}}

# Check LLT source formatting (exit 1 if unformatted)
fmt-llt-check FILE:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin tinct -- fmt --check {{FILE}}

# Format LLT source file in place
fmt-llt-fix FILE:
    {{container}} run {{run_flags}} {{rust_image}} cargo run --bin tinct -- fmt --in-place {{FILE}}

# Show Rust version
version:
    {{container}} run {{run_flags}} {{rust_image}} rustc --version
    {{container}} run {{run_flags}} {{rust_image}} cargo --version

# Build documentation
doc:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo doc --no-deps

# Build and open documentation
doc-open:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo doc --no-deps --open

# Run cargo bench (if benchmarks exist)
bench:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo bench

# Audit dependencies for security vulnerabilities
audit:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} sh -c "cargo install cargo-audit@0.22.1 --locked && cargo audit"

# Watch for changes and run tests (requires cargo-watch)
watch:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} sh -c "cargo install cargo-watch && cargo watch -x test"

# Remove all container images (cleanup)
clean-images:
    {{container}} rmi {{rust_image}} || true

# Remove build volumes (WARNING: clears all build cache)
clean-volumes:
    {{container}} volume rm {{project_name}}-target {{project_name}}-cargo || true

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
