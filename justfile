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

# Propagate TIMEOUT env var to podman as --timeout=N (seconds), or omit if unset
timeout_flag := if env_var_or_default("TIMEOUT", "") != "" { " --timeout=" + env_var_or_default("TIMEOUT", "") } else { "" }

# Container memory limit applied via --memory.
# Also used to compute tinct's --max-memory (RLIMIT_AS) at 95% of this value.
container_memory := "10g"

# 95% of container_memory in bytes for tinct --max-memory.
# 10g (binary) = 10 × 2³⁰ = 10,737,418,240 bytes; × 0.95 = 10,200,547,328.
# Update both when container_memory changes.
tinct_max_memory := "10200547328"

# Common container run flags
# target/ is a bind mount so binaries land on the host (symlinkable from ~/.local/bin)
# cargo registry cache stays a named volume — no need to expose it on the host
run_flags := "--rm --memory " + container_memory + timeout_flag + " -v .:/workspace:z -v ./target:/workspace/target:z -v " + project_name + "-cargo:/usr/local/cargo/registry -w /workspace"

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
test: build-release
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --lib -- --test-threads=1
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --test corpus_tests -- --test-threads=1
    {{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --test cli_tests
    {{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --test integration_async
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --features lsp --test lsp_corpus_tests -- --test-threads=1

# Run a specific test
test-one TEST:
    {{container}} run {{run_flags}} {{rust_image}} cargo test {{TEST}}


# Run only lib unit tests (no integration tests)
# --test-threads=1 prevents OOM from parallel stdlib cache accumulation (same as `just test`)
test-lib:
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --lib -- --test-threads=1

# Run lib tests and show only failures + summary lines
test-lib-summary:
    -{{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 -e RUSTFLAGS="-D warnings" {{rust_image}} sh -c "cargo test --lib -- --test-threads=1 2>&1 | grep -E 'FAILED|test result:|failures:'"

# Run corpus tests and show only failures + summary lines
test-corpus-summary:
    -{{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 -e RUSTFLAGS="-D warnings" {{rust_image}} sh -c "cargo test --test corpus_tests -- --test-threads=1 2>&1 | grep -E 'FAILED|test result:|failures:'"

# Run CLI tests and show only failures + summary lines
test-cli-summary:
    -{{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} sh -c "cargo test --test cli_tests 2>&1 | grep -E 'FAILED|test result:|failures:'"

# Run LSP corpus tests and show only failures + summary lines
test-lsp-summary:
    -{{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 -e RUSTFLAGS="-D warnings" {{rust_image}} sh -c "cargo test --features lsp --test lsp_corpus_tests -- --test-threads=1 2>&1 | grep -E 'FAILED|test result:|failures:'"


# Run only corpus tests (NOTE: does not include LSP corpus tests — use `just test-lsp` for those)
test-corpus:
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 {{rust_image}} cargo test --test corpus_tests

# Update corpus test expected outputs to match actual evaluator output.
# Usage:
#   just update-corpus                          # update all eval corpus tests
#   just update-corpus --dry-run                # preview changes
#   just update-corpus --filter "identity"      # only matching files
#   just update-corpus --all                    # eval + valid + typecheck
update-corpus *ARGS:
    #!/usr/bin/env bash
    set -euo pipefail
    DRY_RUN=""
    FILTER=""
    TESTS="update_eval_corpus"
    for arg in {{ARGS}}; do
        case "$arg" in
            --dry-run) DRY_RUN="-e UPDATE_CORPUS_DRY_RUN=1" ;;
            --filter) shift_next=1 ;;
            --all) TESTS="update_eval_corpus update_valid_corpus update_typecheck_warnings_corpus" ;;
            --valid) TESTS="update_valid_corpus" ;;
            --typecheck) TESTS="update_typecheck_warnings_corpus" ;;
            *)
                if [ "${shift_next:-}" = "1" ]; then
                    FILTER="-e UPDATE_CORPUS_FILTER=$arg"
                    shift_next=0
                fi
                ;;
        esac
    done
    for test in $TESTS; do
        echo "=== Running $test ==="
        {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 $DRY_RUN $FILTER {{rust_image}} \
            cargo test --test update_corpus -- --ignored --nocapture --test-threads=1 "$test"
    done

# Run only LSP corpus tests (requires --features lsp)
test-lsp:
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 {{rust_image}} cargo test --features lsp --test lsp_corpus_tests -- --test-threads=1

# Run all lint checks. Always runs every check regardless of failures; exits non-zero if any failed.
lint:
    #!/usr/bin/env bash
    failed=0
    step() {
        echo ""
        just "$1" || { echo "❌ $1 FAILED"; failed=1; }
    }
    just lint-clippy || { echo "❌ lint-clippy FAILED"; failed=1; }
    step lint-clippy-allows
    step lint-inner-allows
    step lint-cfg-attr-allow
    step lint-expect-attrs
    step lint-stdlib
    step lint-docs
    step lint-md
    step lint-builtins-cps
    echo ""
    if [ "$failed" -ne 0 ]; then
        echo "❌ One or more lint checks failed — see output above"
        exit 1
    fi
    echo "✅ All lint checks passed!"

lint-clippy:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rustup component add clippy 2>/dev/null; cargo clippy --tests -- -D warnings"

# Outer attributes: #[allow(...)]. The ! in #![allow is NOT matched here — see lint-inner-allows.
lint-clippy-allows:
    @echo "=== MANUAL REVIEW: #[allow] suppressions ==="
    @echo "Evaluate each item below. Any #allows must be used very sparingly, make sure each one is justified:"
    @echo "  - If the code IS used in tests, the compiler wouldn't warn — delete the allow."
    @echo "  - If the code is NOT used anywhere, delete the code (not just the allow)."
    @echo "  - If it's pending-feature scaffolding, track it in TODO.md and delete until needed."
    @echo "  - Allowing disallowed methods must be done very carefully, this is a security boundary. Verify this usage can't possibly leverage an existing cap"
    @fgrep -B1 -A1 -rn '#[allow' src/ || true

# Inner/file-level attributes: #![allow(...)]. These suppress warnings for the entire file.
# Not caught by lint-clippy-allows because fgrep '#[allow' doesn't match '#![allow'.
lint-inner-allows:
    @echo "=== MANUAL REVIEW: #![allow] file-level suppressions ==="
    @echo "File-level allows suppress a warning class for the entire file. Each must be justified:"
    @echo "  - Prefer targeted #[allow] on the specific item over a blanket file-level allow."
    @echo "  - If the warning is a false positive across the whole file, document why."
    @fgrep -B1 -A1 -rn '#![allow' src/ || true

# Conditional allows: #[cfg_attr(*, allow(*))]. Suppress only under specific configurations.
# More subtle than direct allows — the suppression may only be visible in some build profiles.
lint-cfg-attr-allow:
    @echo "=== MANUAL REVIEW: #[cfg_attr(*, allow(*))] conditional suppressions ==="
    @echo "These suppress warnings only under specific cfg conditions (e.g. test, feature gates)."
    @echo "Verify the allow is still needed and that the condition is as narrow as possible."
    @grep -B1 -A1 -rn 'cfg_attr.*allow' src/ || true

# Expect attributes: #[expect(clippy::*)]. Rust 1.81+ form — errors if the lint does NOT fire.
# Safer than #[allow] (fails if suppression becomes stale) but still needs justification.
lint-expect-attrs:
    @echo "=== MANUAL REVIEW: #[expect] suppressions ==="
    @echo "#[expect] is safer than #[allow] (errors if lint stops firing) but still needs review."
    @echo "Verify the suppressed lint is a genuine false positive, not a real issue."
    @fgrep -B1 -A1 -rn '#[expect' src/ || true

# Verify no builtins call materialize() directly on args (all forced args must use force_count).
# Annotation conventions for intentional exceptions:
#   // H1: <reason>  — unconditional force, needs force_count migration (known debt)
#   // H2: <reason>  — conditional materialize, safe pattern (e.g., only one branch)
#   // H3: <reason>  — loop materialize, safe pattern (e.g., iterating seq elements)
#   // TEST:         — test-only code
# Runs without container — it's a grep, not a build.
# Patterns caught:
#   materialize(&args[N]     — positional index via slice index
#   materialize(&args.args[N — field-access via BuiltinArgs.args[N]
#   materialize(X_thunk,     — variable extracted from args.args.as_slice() destructuring
lint-builtins-cps:
    @! grep -Ern 'materialize\(&args(\.args)?\[' src/builtins*.rs | grep -v '// H1:\|// H2:\|// H3:\|// TEST:' || (echo "ERROR: builtins still call materialize directly (unannotated call)" && exit 1)
    @! grep -Ern 'materialize\(\w+_thunk,' src/builtins*.rs | grep -v '// H1:\|// H2:\|// H3:\|// TEST:' || (echo "ERROR: builtins still call materialize directly on extracted thunk (unannotated call)" && exit 1)
    @echo "OK: No unannotated builtins call materialize() directly"

# Run tinct lint --strict on every .llt file in stdlib/
lint-stdlib: build-release
    find stdlib -name '*.llt' -type f -print -exec \
        {{container}} run {{run_flags}} {{rust_image}} ./target/release/tinct lint --strict {} \;

# Type-check tinct code blocks in documentation using tinct literate lint.
# Only includes docs where all code blocks are expected to type-check cleanly.
# Add more files here as they are verified.
lint-docs: build-release
    {{container}} run {{run_flags}} {{rust_image}} ./target/release/tinct literate lint doc/quickstart.md

# Lint all markdown files with markdownlint-cli2
lint-md:
    {{container}} run --rm{{timeout_flag}} -v .:/workspace:z -w /workspace {{node_image}} \
        sh -c "npx --yes markdownlint-cli2 'doc/**/*.md' 'README.md'"

# Auto-fix markdown lint issues (not all rules are auto-fixable)
lint-md-fix:
    {{container}} run --rm{{timeout_flag}} -v .:/workspace:z -w /workspace {{node_image}} \
        sh -c "npx --yes markdownlint-cli2 --fix 'doc/**/*.md' 'README.md'"

# Run clippy with auto-fixes
lint-fix:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rustup component add clippy 2>/dev/null; cargo clippy --tests --fix --allow-dirty --allow-staged"

# Check code formatting
fmt-check:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rustup component add rustfmt 2>/dev/null; cargo fmt -- --check"

# Format code
fmt:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rustup component add rustfmt 2>/dev/null; cargo fmt"

# Run a tinct file
run-file FILE:
    {{container}} run {{run_flags}} {{rust_image}} cargo run --bin tinct -- --max-memory {{tinct_max_memory}} run {{FILE}}

# Clean build artifacts
clean:
    {{container}} run {{run_flags}} {{rust_image}} cargo clean

# Check if the code compiles without building
check:
    {{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} cargo check

# Show only error lines from a build (for CI / large output environments)
build-errors:
    -{{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} sh -c "cargo build --message-format short 2>&1 | grep -E 'error(\[E|:)' | grep -v 'previous errors' | head -8"

# Check test code compilation (includes #[cfg(test)] modules) and show first errors
check-test-errors:
    -{{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} sh -c "cargo test --lib --no-run --message-format short 2>&1 | grep -E 'error(\[E|:)' | grep -v 'previous errors' | head -30"

# Pin a specific dependency version
update-precise PKG VER:
    {{container}} run {{run_flags}} {{rust_image}} cargo update {{PKG}} --precise {{VER}}

# Show dependency tree
tree:
    {{container}} run {{run_flags}} {{rust_image}} cargo tree

# Lint a single tinct source file
lint-file FILE: build-release
    {{container}} run {{run_flags}} {{rust_image}} ./target/release/tinct lint --strict {{FILE}}

# Run full CI pipeline (check, test, lint, fmt-check, audit)
ci:
    {{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} cargo check
    just test
    just lint-stdlib
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
ts_run_flags := "--rm" + timeout_flag + " -v .:/workspace:z -w /workspace/tree-sitter-llt"

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
    {{container}} run --rm{{timeout_flag}} -v .:/workspace:z -w /workspace/integrations/vscode {{node_image}} sh -c "npm install && npm run compile && npx @vscode/vsce package --no-dependencies"

# Format LLT source file and print to stdout (pretty formatter, default)
fmt-llt FILE:
    {{container}} run {{run_flags}} {{rust_image}} cargo run --bin tinct -- fmt {{FILE}}

# Format LLT source file with compact (single-line) output
fmt-llt-compact FILE:
    {{container}} run {{run_flags}} {{rust_image}} cargo run --bin tinct -- fmt -o compact {{FILE}}

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
# Writes samples/versions-spans.llt-stream (raw profile) and samples/versions-trace.json (Perfetto).
versions:
    {{container}} run {{run_flags}} --network=host \
        -e RUST_VERSION={{rust_version}} \
        {{rust_image}} sh -c "ulimit -s unlimited && cargo run --bin tinct -- --max-memory {{tinct_max_memory}} run --cap-net nc=static.rust-lang.org:443 --cap-net nc=crates.io:443 --profile samples/versions-spans.llt-stream samples/versions.llt && cargo run --bin tinct -- --max-memory {{tinct_max_memory}} run -i stream -o json --strict scripts/profile/trace.llt < samples/versions-spans.llt-stream > samples/versions-trace.json"

# Generate stdlib API reference from @[doc: "..."] annotations.
# Writes one file per module to doc/lib/<module>.md.
# The module index is now maintained manually in doc/11-stdlib.md §Supplemental Module Reference.
docgen:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "mkdir -p doc/lib && cargo run --bin tinct -- --max-memory {{tinct_max_memory}} run --strict --cap-fs docdir=doc/lib:w scripts/docgen.llt"

# Weave tinct code block outputs into doc/*.md (living documentation).
# Updates the === out / === warn / === info sections inside each ```tinct block.
# Re-run to update. Use `just doc-verify` in CI to check without modifying.
doc: build-release
    for f in doc/*.md; do \
        {{container}} run {{run_flags}} {{rust_image}} ./target/release/tinct literate weave --strict -i "$f"; \
    done

# Verify that all annotated ```tinct blocks in doc/*.md match their === expected sections.
# Exits 0 if all match, exits 1 with diff details if any mismatch. Use in CI.
doc-verify: build-release
    for f in doc/*.md; do \
        {{container}} run {{run_flags}} {{rust_image}} ./target/release/tinct literate weave --strict --fail-on-errors --verify "$f"; \
    done

# Build Rust API documentation
rust-doc:
    {{container}} run {{run_flags}} {{rust_image}} cargo doc --no-deps

# Run cargo bench (if benchmarks exist)
bench:
    {{container}} run {{run_flags}} {{rust_image}} cargo bench

# Generate LLVM coverage report
# Output: target/llvm-cov/html/index.html
coverage:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rustup component add llvm-tools-preview 2>/dev/null; cargo install cargo-llvm-cov --locked && cargo llvm-cov --html"

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

# Audit dependencies for security vulnerabilities
audit:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "cargo install cargo-audit@0.22.1 --locked && cargo audit"

# List every callsite that suppresses the cap-std enforcement lints so that each one
# can be reviewed on every CI run. Enforcement is via clippy::disallowed_methods /
# clippy::disallowed_types — this recipe is a human-review reminder only.
check-ambient-dir:
    @echo "=== cap-std lint suppressions — verify each allow is still justified ==="
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rg '#\[allow\(clippy::disallowed' src/ --type rust -n || echo '(none)'"

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

# Profile a tinct file and show materialization hotspots
profile FILE:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "cargo run --release -- --max-memory {{tinct_max_memory}} run --profile /tmp/spans.llt-stream {{FILE}} && cargo run --release -- --max-memory {{tinct_max_memory}} run -i stream scripts/profile/materialize.llt < /tmp/spans.llt-stream"

# Profile a tinct file and output Perfetto trace
profile-trace FILE:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "cargo run --release -- --max-memory {{tinct_max_memory}} run --profile /tmp/spans.llt-stream {{FILE}} && cargo run --release -- --max-memory {{tinct_max_memory}} run -i stream -o json scripts/profile/trace.llt < /tmp/spans.llt-stream"

