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
test: build-release
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --lib -- --test-threads=1
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --test corpus_tests -- --test-threads=1
    {{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --test cli_tests
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --features lsp --test lsp_corpus_tests -- --test-threads=1
    for f in stdlib/**/*.llt stdlib/*.llt; do \
        {{container}} run {{run_flags}} {{rust_image}} ./target/release/tinct lint --no-fs "$f" || exit 1; \
    done

# Run tests with output (NOTE: LSP corpus tests require `just test-lsp` — they need --features lsp)
test-verbose:
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 {{rust_image}} cargo test -- --nocapture

# Run a specific test
test-one TEST:
    {{container}} run {{run_flags}} {{rust_image}} cargo test {{TEST}}

# Run only lib unit tests (no integration tests)
test-lib:
    {{container}} run {{run_flags}} -e RUSTFLAGS="-D warnings" {{rust_image}} cargo test --lib

# Run only corpus tests (NOTE: does not include LSP corpus tests — use `just test-lsp` for those)
test-corpus:
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 {{rust_image}} cargo test --test corpus_tests

# Run only LSP corpus tests (requires --features lsp)
test-lsp:
    {{container}} run {{run_flags}} -e RUST_MIN_STACK=67108864 {{rust_image}} cargo test --features lsp --test lsp_corpus_tests -- --test-threads=1

# Run clippy (linter) + manual review prompts for #[allow] suppressions and open_ambient_dir
lint:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rustup component add clippy 2>/dev/null; cargo clippy -- -D warnings"
    @echo ""
    @echo "=== MANUAL REVIEW: #[allow] suppressions ==="
    @echo "Evaluate each item below. #[allow(dead_code)] is a code smell:"
    @echo "  - If the code IS used in tests, the compiler wouldn't warn — delete the allow."
    @echo "  - If the code is NOT used anywhere, delete the code (not just the allow)."
    @echo "  - If it's pending-feature scaffolding, track it in TODO.md and delete until needed."
    @echo "  - Legitimate exceptions: clippy style suppressions (too_many_arguments, type_complexity, etc.)"
    @fgrep -rn '#[allow' src/ || true
    @echo ""
    @echo "=== MANUAL REVIEW: cap-widening directory access ==="
    @echo "Evaluate each open_ambient_dir call below. Each one acquires ambient OS authority."
    @echo "Policy: only src/main.rs and src/repl.rs may call open_ambient_dir in production."
    @echo "Any call outside those two files (excluding #[cfg(test)] blocks) is a violation."
    @echo "Check: is the path operator-controlled? Is this the bootstrap boundary? Document why."
    @fgrep -rn 'open_ambient_dir' src/ || true
    @echo ""
    @echo "=== STDLIB LINT ==="
    @just lint-stdlib-strict

# Run tinct lint --strict on every .llt file in stdlib/
lint-stdlib-strict: build-release
    for f in stdlib/*.llt stdlib/*/*.llt stdlib/*/*/*.llt stdlib/*/*/*/*.llt; do \
        [ -f "$$f" ] || continue; \
        echo "  lint: $$f"; \
        {{container}} run {{run_flags}} {{rust_image}} ./target/release/tinct lint --strict "$$f" || exit 1; \
    done

# Run clippy with auto-fixes
lint-fix:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rustup component add clippy 2>/dev/null; cargo clippy --fix --allow-dirty --allow-staged"

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

# Lint all stdlib .llt files for type errors without executing them
lint-stdlib: build-release
    for f in stdlib/*.llt stdlib/*/*.llt stdlib/*/*/*.llt stdlib/*/*/*/*.llt; do \
        [ -f "$$f" ] || continue; \
        {{container}} run {{run_flags}} {{rust_image}} ./target/release/tinct lint --no-fs "$$f" || exit 1; \
    done

# Lint a single tinct source file
lint-file FILE: build-release
    {{container}} run {{run_flags}} {{rust_image}} ./target/release/tinct lint {{FILE}}

# Run full CI pipeline (check, test, lint-stdlib, lint, fmt-check, audit)
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
    {{container}} run --rm -v .:/workspace:z -w /workspace/integrations/vscode {{node_image}} sh -c "npm install && npm run compile && npx @vscode/vsce package --no-dependencies"

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
# Runs on host (no container overhead) — requires tinct in PATH (just install).
versions:
    {{container}} run {{run_flags}} --network=host \
        -e RUST_VERSION={{rust_version}} \
        {{rust_image}} sh -c "ulimit -s unlimited && cargo run --quiet --bin tinct -- run --strict --cap-net nc=static.rust-lang.org:443 --cap-net nc=crates.io:443 samples/versions.llt"

# Generate stdlib API reference from @[doc: "..."] annotations.
# Writes one file per module to doc/lib/<module>.md.
# The module index is now maintained manually in doc/11-stdlib.md §Supplemental Module Reference.
docgen:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "mkdir -p doc/lib && cargo run --quiet --bin tinct -- run --cap-fs docdir=doc/lib:w scripts/docgen.llt"

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

# Build and open Rust API documentation
rust-doc-open:
    {{container}} run {{run_flags}} {{rust_image}} cargo doc --no-deps --open

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

# Check that open_ambient_dir is only used in the designated capability boundary files
# (src/main.rs, src/repl.rs, src/lib.rs, and src/builtins.rs for bootstrap context
# loading) or inside #[cfg(test)] / lines annotated with // AMBIENT-OK.
# Any production use outside these files violates the cap-std capability boundary policy.
check-ambient-dir:
    {{container}} run {{run_flags}} {{rust_image}} sh -c "rg 'open_ambient_dir' src/ --glob '!src/main.rs' --glob '!src/repl.rs' --glob '!src/lib.rs' --glob '!src/builtins.rs' --type rust | grep -v '// AMBIENT-OK' | grep -v '#\[cfg(test)\]' && echo 'FAIL: open_ambient_dir found outside designated bootstrap files' && exit 1 || true"

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

# ---------------------------------------------------------------------------
# Git rebase helpers — COMPLETED 2026-05-20, runtime-v2 rebased onto main
# Recipes below are for reference; worktree at:
# /var/home/adenton/Projects/tinct.worktree/pr/runtime-v2
# ---------------------------------------------------------------------------

_rv2 := "/var/home/adenton/Projects/tinct.worktree/pr/runtime-v2"

# Create a new worktree under .claude/worktrees/ for rebase work (writable by toolbox)
rebase-setup-local:
    @if git worktree list | grep -q ".claude/worktrees/rv2-rebase"; then \
        echo "Local worktree already exists"; \
    else \
        git worktree add .claude/worktrees/rv2-rebase runtime-v2 2>/dev/null || \
        (git worktree add .claude/worktrees/rv2-rebase --no-checkout && \
         git -C .claude/worktrees/rv2-rebase checkout runtime-v2); \
    fi
    @echo "Worktree: .claude/worktrees/rv2-rebase"

# Stash changes in runtime-v2 worktree (before starting rebase)
rebase-stash:
    cd {{_rv2}} && git stash

# Start the rebase in the runtime-v2 worktree
rebase-start:
    cd {{_rv2}} && git rebase main

# Show rebase status
rebase-status:
    @cd {{_rv2}} && git status
    @echo "---"
    @cd {{_rv2}} && git diff --name-only --diff-filter=U 2>/dev/null || true

# List conflicted files
rebase-conflicts:
    cd {{_rv2}} && git diff --name-only --diff-filter=U

# Show diff for a specific file
rebase-diff-file FILE:
    cd {{_rv2}} && git diff -- {{FILE}}

# Take our version (HEAD/main) for a conflicted file
rebase-ours FILE:
    cd {{_rv2}} && git checkout --ours -- {{FILE}}

# Take their version (runtime-v2 commit) for a conflicted file
rebase-theirs FILE:
    cd {{_rv2}} && git checkout --theirs -- {{FILE}}

# Add a resolved file
rebase-add FILE:
    cd {{_rv2}} && git add -- {{FILE}}

# Add all resolved files
rebase-add-all:
    cd {{_rv2}} && git add -u

# Continue the rebase after resolving conflicts
rebase-continue:
    cd {{_rv2}} && GIT_EDITOR=true git rebase --continue

# Skip a commit during rebase
rebase-skip:
    cd {{_rv2}} && git rebase --skip

# Abort the rebase
rebase-abort:
    cd {{_rv2}} && git rebase --abort

# Force push the rebased runtime-v2 branch
rebase-push:
    cd {{_rv2}} && git push --force-with-lease origin runtime-v2

# Show log of commits unique to runtime-v2 vs main
rebase-log:
    cd {{_rv2}} && git log --oneline main..HEAD

# Build from the runtime-v2 worktree
rebase-build:
    cd {{_rv2}} && podman run --rm --memory 8g -v .:/workspace:z -v ./target:/workspace/target:z -v tinct-cargo:/usr/local/cargo/registry -w /workspace -e 'RUSTFLAGS=-D warnings' rust:1.95 cargo build

# Replace surface_fields.rs entirely with the origin/runtime-v2 version (cleanest fix)
rebase-fix-surface-fields-replace:
    @echo "Replacing surface_fields.rs with origin/runtime-v2 version..."
    git show origin/runtime-v2:src/surface_fields.rs > {{_rv2}}/src/surface_fields.rs
    @echo "Done. Functions:"
    cd {{_rv2}} && grep -n '^pub fn' src/surface_fields.rs

# Replace value.rs with origin/runtime-v2 version
rebase-fix-value:
    @echo "Replacing value.rs with origin/runtime-v2 version..."
    git show origin/runtime-v2:src/value.rs > {{_rv2}}/src/value.rs
    @echo "Done. Checking for new_ast_node_field:"
    cd {{_rv2}} && grep -n 'pub fn new_ast_node_field' src/value.rs

# Replace ast.rs with origin/runtime-v2 version (cleanest fix for duplicate types)
rebase-fix-ast:
    @echo "Replacing ast.rs with origin/runtime-v2 version..."
    git show origin/runtime-v2:src/ast.rs > {{_rv2}}/src/ast.rs
    @echo "Done. Checking for SurfaceNode:"
    cd {{_rv2}} && grep -n 'pub struct SurfaceNode\|pub enum SurfaceExpression\|pub enum CoreExpr' src/ast.rs

# Fix duplicate Value::Program/Document/Expression match arms in lib.rs
rebase-fix-lib-match:
    @echo "Checking for duplicate match arms..."
    cd {{_rv2}} && grep -n 'Value::Program\|Value::Document\|Value::Expression' src/lib.rs
    @echo "Removing duplicate match arms (lines 798-809 = second set)..."
    cd {{_rv2}} && awk 'NR==798,NR==809{next} {print}' src/lib.rs > src/lib.rs.tmp && mv src/lib.rs.tmp src/lib.rs
    @echo "After fix:"
    cd {{_rv2}} && grep -n 'Value::Program\|Value::Document\|Value::Expression' src/lib.rs

# Commit post-rebase fixes and force push
rebase-commit-and-push:
    cd {{_rv2}} && git add -u
    cd {{_rv2}} && git commit -m "rebase-fixup: resolve API mismatches after main→runtime-v2 rebase\n\nThe rebase of runtime-v2 onto main caused API incompatibilities because\nthe two branches had diverged with incompatible changes:\n\n- Replaced key source files with origin/runtime-v2 versions to restore\n  consistent APIs (value.rs, eval.rs, eval_materialize.rs, resolve.rs,\n  builtins_meta.rs, lib.rs, eval_dict.rs, eval_call.rs, builtins.rs,\n  eval_access.rs, eval_pipeline.rs, expand.rs, builtins_math.rs,\n  builtins_string.rs, surface_fields.rs, ast.rs, type_normalize.rs)\n- Fixed duplicate module declarations (surface_fields, ast_convert) in lib.rs\n- Fixed do_infer_resolutions type (Span->String key) for compatibility\n- Added NominalVariant match arms to eval.rs and type_normalize.rs\n- Build passes: just build exits 0 with -D warnings"
    cd {{_rv2}} && git push --force-with-lease origin runtime-v2

# Replace all core files with origin/runtime-v2 versions to fix API mismatches
rebase-fix-all-core:
    @echo "Replacing core source files with origin/runtime-v2 versions..."
    git show origin/runtime-v2:src/eval.rs > {{_rv2}}/src/eval.rs
    git show origin/runtime-v2:src/eval_materialize.rs > {{_rv2}}/src/eval_materialize.rs
    git show origin/runtime-v2:src/resolve.rs > {{_rv2}}/src/resolve.rs
    git show origin/runtime-v2:src/builtins_meta.rs > {{_rv2}}/src/builtins_meta.rs
    git show origin/runtime-v2:src/lib.rs > {{_rv2}}/src/lib.rs
    @echo "Done. Checking lib.rs module declarations:"
    cd {{_rv2}} && grep -n 'mod surface_fields\|mod ast_convert\|mod lower' src/lib.rs
    @echo "Checking for DefaultFallback in eval.rs:"
    cd {{_rv2}} && grep -c 'DefaultFallback' src/eval.rs || true

# Replace remaining mixed files with origin/runtime-v2 versions
rebase-fix-eval-files:
    @echo "Replacing eval_dict.rs, eval_call.rs, builtins.rs with origin/runtime-v2 versions..."
    git show origin/runtime-v2:src/eval_dict.rs > {{_rv2}}/src/eval_dict.rs
    git show origin/runtime-v2:src/eval_call.rs > {{_rv2}}/src/eval_call.rs
    git show origin/runtime-v2:src/builtins.rs > {{_rv2}}/src/builtins.rs
    @echo "Done."

# Replace more files with origin/runtime-v2 to fix CallContext and other issues
rebase-fix-more-files:
    @echo "Replacing more src files with origin/runtime-v2 versions..."
    git show origin/runtime-v2:src/eval_access.rs > {{_rv2}}/src/eval_access.rs
    git show origin/runtime-v2:src/eval_pipeline.rs > {{_rv2}}/src/eval_pipeline.rs
    git show origin/runtime-v2:src/type_normalize.rs > {{_rv2}}/src/type_normalize.rs
    git show origin/runtime-v2:src/builtins_math.rs > {{_rv2}}/src/builtins_math.rs
    git show origin/runtime-v2:src/builtins_string.rs > {{_rv2}}/src/builtins_string.rs
    git show origin/runtime-v2:src/expand.rs > {{_rv2}}/src/expand.rs
    @echo "Done."

# Fix do_infer_resolutions type mismatch: Span key -> String key in eval.rs
# (origin/runtime-v2 eval.rs uses Span key but type_infer.rs uses String key)
rebase-fix-do-infer:
    @echo "Patching eval.rs to use String key for do_infer_resolutions (matching type_infer.rs)..."
    cd {{_rv2}} && sed -i 's/pub do_infer_resolutions: RefCell<HashMap<Span, String>>/pub do_infer_resolutions: RefCell<HashMap<String, String>>/' src/eval.rs
    cd {{_rv2}} && sed -i 's/pub fn set_do_infer_resolutions.*HashMap<Span, String>/pub fn set_do_infer_resolutions(\&self, resolutions: HashMap<String, String>/' src/eval.rs
    cd {{_rv2}} && sed -i 's/ctx\.do_infer_resolutions\.borrow()\.get(\&expr\.span)/ctx.do_infer_resolutions.borrow().get(name)/' src/eval.rs
    @echo "Checking result:"
    cd {{_rv2}} && grep -n 'do_infer_resolutions.*HashMap' src/eval.rs | head -5

# Fix NominalVariant not covered in eval.rs and type_normalize.rs
# Use origin/runtime-v2 type_normalize.rs (has correct CallContext) and fix NominalVariant in both
rebase-fix-nominal-variant:
    @echo "Replacing type_normalize.rs with origin/runtime-v2 version..."
    git show origin/runtime-v2:src/type_normalize.rs > {{_rv2}}/src/type_normalize.rs
    @echo "Adding NominalVariant to type_normalize.rs Display impl..."
    cd {{_rv2}} && sed -i 's|Type::App(func, arg) => write!(f, "\[{} {}\]", func, arg),|Type::NominalVariant { tag, .. } => write!(f, "{}", tag),\n            Type::App(func, arg) => write!(f, "[{} {}]", func, arg),|' src/type_normalize.rs
    @echo "Restoring eval.rs NominalVariant (removing double-added arm)..."
    git show origin/runtime-v2:src/eval.rs > {{_rv2}}/src/eval.rs
    cd {{_rv2}} && sed -i 's/pub do_infer_resolutions: RefCell<HashMap<Span, String>>/pub do_infer_resolutions: RefCell<HashMap<String, String>>/' src/eval.rs
    cd {{_rv2}} && sed -i 's/pub fn set_do_infer_resolutions.*HashMap<Span, String>/pub fn set_do_infer_resolutions(\&self, resolutions: HashMap<String, String>/' src/eval.rs
    cd {{_rv2}} && sed -i 's/ctx\.do_infer_resolutions\.borrow()\.get(\&expr\.span)/ctx.do_infer_resolutions.borrow().get(name)/' src/eval.rs
    cd {{_rv2}} && sed -i 's|// Type constructor application and variables: treat like TypeVar (accept any value)|// NominalVariant: check if value is a Variant with matching tag\n        Type::NominalVariant { tag, .. } => {\n            matches!(value, Value::Variant { tag: v_tag, .. } if v_tag == tag)\n        }\n        // Type constructor application and variables: treat like TypeVar (accept any value)|' src/eval.rs
    @echo "Checking result:"
    cd {{_rv2}} && grep -c 'NominalVariant' src/eval.rs
    cd {{_rv2}} && grep -c 'NominalVariant' src/type_normalize.rs

# Add missing surface_expr_field_names and Core types from origin/runtime-v2
rebase-fix-missing-types:
    @echo "=== Adding CoreExpr and related types to ast.rs ==="
    git show origin/runtime-v2:src/ast.rs | awk '/^\/\/ runtime-v2 AST types/,0' >> {{_rv2}}/src/ast.rs
    @echo "CoreExpr types added:"
    cd {{_rv2}} && grep -n 'pub enum CoreExpr\|pub struct CoreEntry\|pub struct CoreNamedArg\|pub struct CoreParam\|pub struct CoreMatchArm' src/ast.rs
    @echo "=== Adding missing functions to surface_fields.rs ==="
    git show origin/runtime-v2:src/surface_fields.rs | awk '/^pub fn surface_decl_tag/,0' >> {{_rv2}}/src/surface_fields.rs
    @echo "Functions added:"
    cd {{_rv2}} && grep -n '^pub fn' src/surface_fields.rs

# Fix duplicate builtin_expand in builtins_meta.rs
rebase-fix-builtins-meta:
    @echo "Removing duplicate builtin_expand at line ~1369 (stub) in builtins_meta.rs..."
    cd {{_rv2}} && awk 'BEGIN{count=0} \
        /^pub\(crate\) fn builtin_expand/ { \
            count++; \
            if (count == 2) { skip=1 } \
        } \
        skip && /^}$/ { skip=0; next } \
        !skip {print}' src/builtins_meta.rs > src/builtins_meta.rs.tmp
    @echo "Lines in original: $(wc -l < {{_rv2}}/src/builtins_meta.rs)"
    cd {{_rv2}} && mv src/builtins_meta.rs.tmp src/builtins_meta.rs
    @echo "Lines after fix: $(wc -l < {{_rv2}}/src/builtins_meta.rs)"
    cd {{_rv2}} && grep -n 'pub(crate) fn builtin_expand' src/builtins_meta.rs

# Fix duplicate module declarations in lib.rs added by rebase
rebase-fix-lib:
    @echo "Before fix:"
    cd {{_rv2}} && grep -n 'mod surface_fields\|mod ast_convert\|mod lower' src/lib.rs
    @echo "Removing duplicate lines 83-86 (runtime-v2 comment+decl for surface_fields and ast_convert)..."
    cd {{_rv2}} && sed -i -e '/^\/\/ runtime-v2: surface AST field extraction for match dispatch and dot-access\./d' \
        -e '/^pub(crate) mod surface_fields;$/{ N; /^\(pub(crate) mod surface_fields;\n\)*/{ /runtime-v2/d } }' src/lib.rs
    @echo "Applying targeted fix for duplicate surface_fields and ast_convert..."
    cd {{_rv2}} && awk 'BEGIN{sf=0;ac=0} \
        /^pub\(crate\) mod surface_fields;/{sf++; if(sf>1){next}} \
        /^pub\(crate\) mod ast_convert;/{ac++; if(ac>1){next}} \
        {print}' src/lib.rs > src/lib.rs.tmp && mv src/lib.rs.tmp src/lib.rs
    cd {{_rv2}} && sed -i '/^\/\/ runtime-v2: surface AST field extraction for match dispatch and dot-access\./d' src/lib.rs
    cd {{_rv2}} && sed -i '/^\/\/ runtime-v2: bridge converter from old File\/Expr AST to SurfaceProgram (transitional)\./d' src/lib.rs
    @echo "After fix:"
    cd {{_rv2}} && grep -n 'mod surface_fields\|mod ast_convert\|mod lower' src/lib.rs

# Show current rebase progress (which commit we're on)
rebase-progress:
    @cat {{_rv2}}/.git/rebase-merge/msgnum 2>/dev/null && cat {{_rv2}}/.git/rebase-merge/end 2>/dev/null && echo "commits done/total" || echo "No rebase in progress"
