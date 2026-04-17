# Lazy Lisp Transformer - Containerized Build System
# All commands run in containers, no local Rust installation required

# Container runtime (podman or docker)
container := "podman"

# Rust version to use
rust_version := "1.85"

# Container image
rust_image := "rust:" + rust_version

# Project name for volume naming
project_name := "lazy-lisp-transformer"

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

# Run all tests
test:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo test

# Run tests with output
test-verbose:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo test -- --nocapture

# Run a specific test
test-one TEST:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo test {{TEST}}

# Run only corpus tests
test-corpus:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo test --test corpus_tests

# Run clippy (linter)
lint:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} sh -c "rustup component add clippy 2>/dev/null; cargo clippy -- -D warnings"

# Run clippy with auto-fixes
lint-fix:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo clippy --fix --allow-dirty --allow-staged

# Check code formatting
fmt-check:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} sh -c "rustup component add rustfmt 2>/dev/null; cargo fmt -- --check"

# Format code
fmt:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} sh -c "rustup component add rustfmt 2>/dev/null; cargo fmt"

# Run the application with test_input.txt (eval, JSON output)
run:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin llt -- eval test_input.txt

# Run the application with custom input file
run-file FILE:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin llt -- eval {{FILE}}

# Run with LLT display format
run-llt FILE:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin llt -- eval -f llt {{FILE}}

# Run with piped JSON stdin
run-json JSON FILE:
    echo '{{JSON}}' | {{container}} run -i {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin llt -- eval {{FILE}}

# Run the release build
run-release:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo run --bin llt --release -- eval test_input.txt

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

# Show dependency tree
tree:
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} cargo tree

# Run full CI pipeline (check, test, lint, fmt-check)
ci: check test lint fmt-check
    @echo "✅ All CI checks passed!"

# Interactive shell in container (for debugging)
shell:
    {{container}} run -it {{run_flags}} {{rust_image}} /bin/bash

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
    {{container}} run {{run_flags}} {{user_flag}} {{rust_image}} sh -c "cargo install cargo-audit@0.21.2 && cargo audit"

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
