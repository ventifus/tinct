# Tooling

## Formatter (`tinct fmt`)

**Zero-configuration** code formatter for Tinct files.

The current formatter (`src/formatter.rs`) is an AST-based formatter that walks the `Spanned<File>` AST from `ParseOutput`, using comment maps for placement. It applies the line-breaking, comment, and spacing rules described below. See `doc/whatif/completed/parser-rewrite.md` §AST-Based Formatter for the design.

### Line-Breaking: Width + Element Count

A bracket expression `[...]` is rendered on a single line if both conditions are met:
1. The fully-expanded single-line form fits within **80 characters** (including indentation)
2. The expression contains **≤ 4 entries** (key-value pairs or positional values)

If either condition fails, the expression is expanded to one entry per line, indented 2 spaces deeper than the opening bracket. There is no middle ground — expressions are either fully collapsed or fully expanded.

**Exception:** Function parameter lists (`[fn [params...] body]`) and function type parameter lists (`Fn@Return [Params]`) are always rendered on a single line regardless of width, since splitting params across lines hurts readability.

**Entry counting:** The element count applies to the immediate bracket level, not recursively. A nested bracket like `[@[type: Number default: 0] $expr]` counts as 2 entries at the outer level (the annotation dict and `$expr`), regardless of how many entries the inner `[type: Number default: 0]` contains. Each `...` or `...name` rest entry counts as one entry.

**Rationale:** Width-only (gofmt-style) produces unreadable dense lines for dicts with many short entries. Optimal-layout algorithms (Wadler-Lindig) are overkill for Tinct's relatively flat structure. The element count cap of 4 matches the existing stdlib conventions.

### Comment Attachment: Line-Affinity

Comments are attached to code based on their line position:

- **Trailing comment:** A `#` comment on the same line as code stays attached to that code. `x: 5  # the x value` → the comment is part of the `x: 5` entry.
- **Leading comment:** A `#` comment on its own line is attached to the next code line. It is indented to match the code it precedes.
- **Section comment:** A blank line before a leading comment breaks the attachment — the comment becomes a standalone section separator. The blank line is preserved.

### Semicolons: Always Removed

Semicolons are normalized away. They are syntactic sugar for newlines, and the formatter emits the canonical whitespace-separated form. `[x: 1; y: 2]` becomes `[x: 1 y: 2]` (single-line) or two separate lines (multi-line). The stdlib uses zero semicolons — this is the canonical style.

### Configurability: Zero-Config for Canonical Style

The formatter defines one canonical Tinct style with no layout configuration options. CLI flags control I/O behavior and output mode:

**I/O flags:**
- `--check` — exit 1 if any file is not formatted (CI mode)
- `--in-place` — overwrite files in place
- `stdin` (`-` as file argument) — read from stdin, write to stdout

**Compact mode flags:**
- `--oneline` — single-line output (comments stripped, no trailing newline)
- `--nospaces` — minimize inter-token spacing
- `--minimize` — shorthand for `--oneline --nospaces`

**Rationale:** gofmt's zero-config philosophy. One canonical style eliminates bikeshedding. Compact modes are for embedding and piping, not for primary source formatting. Pre-1.0, if a genuine need for layout configurability emerges (e.g. `--width 100`), knobs can be added later. Starting opinionated is easier than tightening.

### Additional Rules

| Rule | Behavior |
|------|----------|
| Indentation | 2 spaces per bracket depth, fixed |
| Key-value spacing | One space after `:` — `key: value` |
| Access chains | Never broken across lines — `$a.b[0].c` stays intact |
| `---` separators | One blank line above and below (no blank before first document) |
| Blank lines | Collapse runs of 2+ to 1. Preserve single blank lines (intentional grouping) |
| Trailing whitespace | Stripped on every line |
| Trailing newline | Single newline at end of file |
| `@` annotations | No spaces around `@` — `x@Number`, `Fn@Return`, never `x @ Number` |
| Quoted strings | Preserved exactly (escapes not normalized; idempotency) |
| Comments in access chains | Cannot occur (compound-atomic grammar); formatter does not handle |

### Compact Formatter Modes

Three compact modes produce space-efficient output for embedding tinct expressions in shell scripts, piping through `-e` strings, or minimizing file size:

| Flag | Behavior |
|------|----------|
| `--oneline` | All output on a single line; comments stripped; no trailing newline; section headers emit `; ` after metadata |
| `--nospaces` | Spaces removed except where required for unambiguous tokenization |
| `--minimize` | Shorthand for `--oneline --nospaces` (maximally compact) |

**Examples:**

```bash
# Normal format (default)
$ tinct fmt config.llt
[x: 1 y: 2]

# Oneline mode: single line, comments stripped
$ tinct fmt --oneline config.llt
[x: 1 y: 2]

# Nospaces mode: minimal inter-token spacing
$ tinct fmt --nospaces config.llt
[x:1 y:2]

# Minimize mode: both oneline and nospaces
$ tinct fmt --minimize config.llt
[x:1 y:2]
```

**Section headers in oneline mode:**

Section headers (`---`) emit as `; ` (semicolon + space) after the header metadata when in oneline mode:

```bash
# Input
[x: 1]
--- %defaults@Dict
[y: 2]

# Oneline output
[x: 1] --- %defaults@Dict; [y: 2]
```

The `---` separator is preserved verbatim even in minimize mode to ensure document structure remains parseable.

**Bare-word adjacency rule (nospaces mode):**

When `--nospaces` is enabled, a space is inserted between two consecutive tokens **only** when both the preceding token's last character **and** the following token's first character are bare-word characters (alphanumeric, `-`, `_`, `?`, `!`, `/`, `%`, `~`).

This rule prevents unintended token merging. For example:

- `[x: 1 y: 2]` → `[x:1 y:2]` — space required between `1` (ends with digit) and `y` (starts with letter)
- `[call f arg]` → `[call f arg]` — all tokens are bare words, spaces required
- `--- %name` → `---%name` would lex as a single bare word, so the space is preserved

**Round-trip guarantee:**

All three modes are re-parseable:

```bash
# Round-trip via oneline
$ tinct fmt --oneline file.llt | tinct run -
(same output as `tinct run file.llt`)

# Idempotency
$ tinct fmt --minimize file.llt | tinct fmt --minimize -
(output unchanged)
```

Comments are stripped in `--oneline` and `--minimize` modes (comments cannot survive without newlines). Section headers with `%name@Type` and `expects: @Type` metadata survive all modes.

### Tinct-Hosted Formatter

The compact and pretty formatters are implemented in `stdlib/formatter/compact.llt` and `stdlib/formatter/pretty.llt`. A full tinct-hosted formatter (`stdlib/formatter/format.llt`) that receives the AST dict (from `ast_to_dict(Some(src), Some(comments))`) as `%` and returns formatted source is not yet implemented. The Rust formatter (`src/formatter.rs`) is retained for LSP use (where loading a tinct program would be too slow).

See `doc/whatif/completed/tinct-hosted-formatter.md` for the full design.

## Inline Expressions and I/O Formatters (`tinct run`)

The `tinct run` command supports inline expressions (`-e`), input formatters (`-i`), and output formatters (`-o`) to enable jq-style JSON processing and flexible pipeline composition.

### `-e <expr>` / `--expr <expr>` — Inline Expressions

Evaluate an inline tinct expression as a pipeline stage. Repeatable — each `-e` occurrence inserts a pipeline stage at that position in the command line, interleaved with file arguments. Each expression receives `%` from the previous stage.

```bash
# Access a field from piped JSON (auto-detection)
tinct run -e '%.x' <<< '{"x":42}'                  # → 42

# Chain multiple expressions
tinct run -e '[x: 1]' -e '[merge % [y: 2]]'       # → {"x":1,"y":2}

# --- is valid inside a single -e string for multiple stages
tinct run -e '[x: 1] --- [y: %.x]'                 # → {"y":1}
```

Semicolons (`;`) are whitespace-equivalent and compress multi-line syntax but do not create pipeline stages.

### `-i <format>` / `--input <format>` — Input Formatters

Prepend an input formatter from `stdlib/in/<format>.llt` as the first pipeline stage. Suppresses stdin JSON auto-detection so the input program reads from the `%stdin` Handle directly. Error if the formatter file does not exist.

```bash
# Explicit JSON input (equivalent to auto-detection but via formatter)
tinct run -i json -e '%.x' <<< '{"x":42}'          # → 42
```

**Convention:** Input formatters live in `stdlib/in/`. Each formatter reads from the `%stdin` Handle and produces a tinct value as `%` for the next stage.

**Included input formatters:**
- `json` — `[from-json [slurp %stdin]]` (parse JSON from %stdin)

When `-i` is present, auto-detection is suppressed and the input program reads from `%stdin` as a Handle (via `$slurp` or `$lines`).

### `-o <format>` / `--output <format>` — Output Formatters

Append an output formatter from `stdlib/out/<format>.llt` as the final pipeline stage. Error if the formatter file does not exist.

```bash
# String output without JSON quotes
tinct run -i json -e '%.msg' -o raw <<< '{"msg":"hello"}'   # → hello
```

**Convention:** Output formatters live in `stdlib/out/`. Each formatter receives `%` and produces formatted output (typically via `$emit` or as the final value).

**Included output formatters:**
- `raw` — Emit strings unquoted; Seq elements one per line; error for other types

### Symmetric Pipeline Model

The three flags compose to form a symmetric pipeline:

```
stdin → [-i input] → [files/exprs] → [-o output] → stdout
```

**Example — jq-style JSON processing:**

```bash
# Extract a field and emit it without quotes
tinct run -i json -o raw -e '%.response' < mcp.json

# Equivalent to jq -r '.response'
```

## Default Output Format (`tinct run`)

When `tinct run` finishes and no `emit` call was made, the final value is serialized to stdout as JSON. This serialization is performed by `stdlib/out/json.llt` — a pure-tinct JSON serializer that ships with the standard library.

**Key properties:**

- The formatter is user-visible and lives at `stdlib/out/json.llt`. You can inspect it or use it directly in programs: `[include %libdir "out/json.llt"]`.
- If `stdlib/out/json.llt` is not found (e.g. running the binary without the stdlib installed), the CLI falls back to a built-in Rust serializer. Note: the fallback serializes empty dicts as `{}` (JSON empty object) rather than `null`.
- The output is indented (2-space pretty-printed) by default.

**Using the formatters directly:**

```bash
tinct run config.llt                  # indented JSON via stdlib/out/json.llt (2-space pretty-printed)
```

```tinct
# Load and call the JSON formatter explicitly in a pipeline
[
  json: [include %libdir "out/json.llt"]
  output: [json.json my-value]
]
```

## VS Code Extension (`just ext`)

A VS Code extension that provides Tinct language support: live diagnostics and hover types via the `tinct lsp` language server.

### Installation

Build from source and install:

```bash
just ext                                              # compile, package → tinct-0.1.0.vsix
code --install-extension tinct-0.1.0.vsix            # install in VS Code
```

After installation, VS Code activates the extension automatically when a `.llt` or `.tinct` file is opened.

### How It Works

The extension spawns `tinct lsp` as a child process and communicates via stdio (LSP protocol). The LSP server provides:

- **Diagnostics** — parse errors and type errors appear as squiggly underlines in real time
- **Hover** — hovering over an expression shows its inferred type
- **Go To Definition** — F12 on a variable reference jumps to the dict entry key that defines it; includes cross-file resolution for `$include` and prelude names

### Configuration

| Setting | Default | Description |
|---------|---------|-------------|
| `tinct.serverPath` | `"tinct"` | Path to the tinct binary. Must be on `PATH`, or set to an absolute path. |

For development (running the LSP server from source without installing a binary):

```json
{
  "tinct.serverPath": "cargo"
}
```

Then set `args` to `["run", "--", "lsp"]` by editing the extension source directly, or set `tinct.serverPath` to a wrapper script that invokes `cargo run -- lsp`.

### Extension Files

The extension lives in `integrations/vscode/`:

| File | Purpose |
|------|---------|
| `package.json` | Extension manifest, language contribution, configuration |
| `language-configuration.json` | Bracket pairs, comment prefix, word pattern |
| `syntaxes/tinct.tmLanguage.json` | TextMate grammar for syntax highlighting |
| `src/extension.ts` | Extension entry point — LSP client wiring |
| `tsconfig.json` | TypeScript build configuration |

## Strict Mode

The `--strict` flag makes type errors fatal instead of advisory. Useful for CI pipelines and pre-commit hooks where type errors should block builds.

**`tinct run --strict`**

Type errors from `typecheck_file()` are collected, printed to stderr, and cause the command to exit with code 1. Without `--strict`, type checking remains advisory — type errors are silently ignored and evaluation proceeds.

```bash
# Type errors are fatal in strict mode
tinct run --strict config.llt
# If config.llt has type errors, exits with code 1 and prints errors to stderr

# Without --strict (default), type errors are advisory
tinct run config.llt
# Evaluation proceeds even if type errors exist
```

**`tinct fmt --strict`**

Format checking fails if the file has type errors. Exits with code 1 before formatting is applied. Without `--strict`, formatting proceeds regardless of type errors.

```bash
# CI pre-commit hook: reject unformatted or type-unsafe code
tinct fmt --strict --check *.llt
```

**Exit codes:**

| Code | Meaning |
|------|---------|
| 0 | Success — no type errors |
| 1 | Type errors detected (strict mode) or other error |

**When strict mode is active:**
- All type errors are printed to stderr in the same format as advisory warnings
- The error count is reported: `type checking failed with N error(s) (--strict mode)`
- Evaluation or formatting does not proceed

## Corpus Test Format

Tinct's corpus tests use a labeled-section format to validate parsing, evaluation, and type checking in a single test file.

### File Extension

Test files use the `.llt-eval` extension to distinguish them from regular `.llt` source files.

### Structure

A test file consists of:
1. Optional directives (first line only, starting with `#`)
2. Tinct source code (the input to test)
3. Labeled sections (delimited by `=== <label>`)

```
# no_fs
[x: 1  y: 2]
=== out
{"x": 1, "y": 2}
=== warn
unused variable: z
```

### Labeled Sections

Tests use `===` (three equals signs) as the delimiter between source code and expected output. The delimiter must be followed by a label.

**Valid labels:**

| Label | Meaning | Assertion when absent |
|-------|---------|----------------------|
| `=== out` | Expected eval output (or AST for `tests/corpus/valid/`) | Test is parse-only (no output check) |
| `=== warn` | Expected type warnings | Assert zero type warnings |
| `=== error` | Expected error substring (for error tests) | Assert zero errors |

**Important:** A bare `===` (without a label) is a parse error. The test runner will panic with the message: `bare '===' is no longer valid; use '=== out', '=== warn', or '=== error'`.

**Convention note:** `tests/corpus/eval/errors/` tests use `=== out` for their expected error substrings — this is a historical convention that predates labeled sections. The `=== error` label is for eval tests *outside* the `errors/` subdirectory that want to assert eval failure (and check an error substring) without using `=== out`. The two conventions coexist: `=== out` in `errors/` tests means "eval fails and output contains this substring"; `=== error` in any test means "eval fails and the error message contains this substring"; `=== out` outside `errors/` means "eval succeeds and output equals this string".

### Output Format

- **`tests/corpus/valid/`**: `=== out` contains the expected AST in Display format (`Expr::fmt`)
- **`tests/corpus/eval/`**: `=== out` contains the expected value in Debug format (`Value::fmt` with `#?`)
- **`tests/corpus/eval/errors/`**: `=== out` contains an expected error substring that must include an `[EXXX]` error code
- **`tests/corpus/invalid/`**: `=== out` contains an expected parse error substring

### Directives

Directives appear on the first line only, starting with `#`. The directive line is stripped from the input before parsing.

**`# no_fs`** — Disable filesystem access for this test. Equivalent to `eval_source_with_config(input, no_fs: true)`. Used for tests that verify `$include` is blocked in sandboxed mode.

**Important:** If the first line starts with `#`, it is treated as a directive line and stripped from the input, even if it's just a comment. To include a comment in the input, place it on line 2 or later.

### Section Order

Sections can appear in any order. The test runner extracts all sections regardless of order.

```
[x: 1]
=== warn
unused variable: y
=== out
{"x": 1}
```

### Zero-Warning Assertion

A test file without a `=== warn` section asserts that type checking produces zero warnings. This is the default expectation for clean code.

To assert that a test *should* produce warnings, include a `=== warn` section with the expected warning substring:

```
[x@Int: "hello"]
=== out
{"x": "hello"}
=== warn
expected Int, found Str
```

### Error Substring Matching

The `=== error` section enables substring matching for error tests. The test passes if the actual error message contains the expected substring.

For `tests/corpus/eval/errors/`, the expected substring must include an `[EXXX]` error code (e.g., `[E001]`, `[E042]`). This ensures error codes are stable across refactoring.

```
[call $error "boom"]
=== out
[E024]
```

The actual error message might be `[E024] explicit error: boom`, but the test only checks for the presence of `[E024]`.

### Example: Multi-Section Test

```
# Test: dict with type error
[
  x@Int: 42
  y@Str: 99
]
=== out
{"x": 42, "y": 99}
=== warn
expected Str, found Int
```

This test:
- Parses successfully
- Evaluates to `{"x": 42, "y": 99}` (output matches `=== out`)
- Produces a type warning about `y` (warning substring matches `=== warn`)

### Migration from Bare `===`

Older corpus tests used a bare `===` delimiter. This is no longer valid. To migrate:

```bash
# Replace bare === with === out
sed -E 's/^===$/=== out/' tests/corpus/**/*.llt-eval
```

### Test Discovery

The corpus test runner recursively finds all `.llt-eval` files under `tests/corpus/` and runs them according to their directory:

| Directory | Test Type | Runner |
|-----------|-----------|--------|
| `tests/corpus/valid/` | Parse-only or AST validation | `test_valid_corpus()` |
| `tests/corpus/invalid/` | Parse error validation | `test_invalid_corpus()` |
| `tests/corpus/eval/` | Eval output validation | `test_eval_corpus()` |
| `tests/corpus/eval/errors/` | Eval error validation | `test_eval_error_corpus()` |
| `tests/corpus/eval/type_errors/` | Type error validation | `test_typecheck_error_corpus_eval()` |
| `tests/corpus/typecheck/warnings/` | Typecheck warning validation | `test_typecheck_warnings_corpus()` |

All runners are in `tests/corpus_tests.rs`.

## Sandboxing & Security

Tinct provides multiple unprivileged sandboxing layers to restrict what evaluation can access. All work without root privileges. Sandbox flags are scoped to the subcommands that use them — for example, `--no-fs` and `--timeout` are `eval` subcommand flags. For the document pipeline model (how sandbox flags interact with multi-document evaluation and `%` pipeline), see [Documents](09-documents.md).

**Implemented features:**
- `--no-fs`: Application-level filesystem blocking (disables `$include` entirely)
- `--timeout <duration>`: SIGALRM-based wall-clock limit (e.g., `--timeout 5s`)
- `--require-integrity`: Require BLAKE3 hashes on all `$include` calls
- **Landlock** (Linux 5.13+): Kernel-enforced filesystem ACLs as defense-in-depth (auto-triggered from `--cap-fs`)
- **seccomp-bpf** (Linux): Network/process syscall blocking
- **rlimit caps**: `--max-memory`, `--max-cpu`, `--max-fds` resource limits
- **Object capability flags**: `--no-pwd`, `--no-libdir`, `--cap-fs NAME=PATH`, `--cap-net NAME=ENTRY`, `--cap-file NAME=PATH:MODE` (injects as `%NAME`)

### Object Capability Model (io-phase1)

The runtime injects three capability values into the root environment at startup. Each represents a specific resource authority; programs that do not receive a capability cannot access that resource.

**Runtime-injected capabilities:**

| Name | Type | Authority | Suppressed by |
|------|------|-----------|---------------|
| `%pwd` | `DirCap` | Current working directory at `tinct run` time | `--no-pwd` |
| `%libdir` | `DirCap` | Tinct standard library directory | `--no-libdir` |
| `%stdin` | `Handle` | File descriptor 0 (standard input) | Only injected when `-i`/`--input` is present |

The `%` prefix on injected cap names makes them visually distinct from user-defined variables. User programs use `%pwd`, `%libdir`, and `%stdin` directly as identifiers (no `$` needed — they are plain bare-word identifiers that happen to start with `%`).

**`--no-pwd`** — Suppresses `%pwd`. Programs that attempt `[open %pwd ...]` or `[include %pwd ...]` receive an undefined variable error. Use for programs that should not access the filesystem even via the working directory.

**`%stdin` injection** — `%stdin` is only injected into the root environment when `-i`/`--input` is present on the command line (indicating a formatter pipeline that reads from stdin). When `-i` is absent, stdin is consumed by the JSON auto-detection path instead. There is no `--no-stdin` flag; stdin access is controlled by the presence or absence of `-i`.

**`--no-libdir`** — Suppresses `%libdir`. Programs that attempt `[include %libdir "io.llt"]` receive an undefined variable error. The embedded stdlib (prelude) is always available via builtins; `--no-libdir` only affects `[include %libdir ...]` calls. Rarely needed — libdir is safe language infrastructure.

**`--cap-fs NAME=PATH`** — Inject an additional named `DirCap`. Creates a directory capability for `PATH` and binds it as `%NAME` in the root environment. Repeatable; each flag adds one cap. Example:

```bash
# %data is a DirCap for /var/data; %out is a DirCap for /tmp/output
tinct run --cap-fs data=/var/data --cap-fs out=/tmp/output script.llt
```

Inside `script.llt`, `%data` and `%out` are available as DirCaps. The program can call `[open %data "config.json" "r"]` but cannot open files outside `/var/data` via `%data`, because the cap's RESOLVE_BENEATH enforcement prevents path traversal.

**`--cap-net NAME=ENTRY`** — Inject a network capability as `%NAME` in the root environment. `ENTRY` is currently a stub; in future it will accept a connector dict or protocol specifier.

**`--cap-file NAME=PATH:MODE`** — Pre-open a single file and inject it as `%NAME` (a Handle) in the root environment. This is a pinpoint capability: the script can only access that one file, not the directory it lives in. Repeatable; each flag adds one Handle.

```bash
# %config is a readable text Handle for Cargo.toml
tinct run --cap-file config=Cargo.toml:r script.llt

# %out is a writable binary Handle for /tmp/output.bin
tinct run --cap-file out=/tmp/output.bin:wb script.llt
```

Mode suffix:
- `r` — read-only, text (`$slurp` returns a String)
- `rb` — read-only, binary (`$slurp` returns Bytes)
- `w` — write-only, text (`$write-handle` writes a String; file is created/truncated)
- `wb` — write-only, binary (`$write-handle` writes Bytes; file is created/truncated)

**`--no-fs`** also suppresses `--cap-file` Handle injection — when `--no-fs` is set, no filesystem caps of any kind are available (`%pwd`, `%libdir`, `--cap-fs`, and `--cap-file` are all blocked).

**`--no-env`** and **`--allow-env NAME`** — Control environment variable access via the `$env` builtin. `--no-env` causes `$env` to return `Null` for all names. `--allow-env NAME` (repeatable) creates an explicit allowlist: only the listed names return their values; all others return `Null`. See §Environment Variable Access.

**Fully sandboxed invocation:**

```bash
# No filesystem caps (not even %pwd), no env vars, 5s timeout
tinct run --no-pwd --no-env --timeout 5s script.llt
```

`%libdir` is retained even in sandboxed invocations so stdlib modules remain accessible. Suppress it explicitly with `--no-libdir` if needed.

**Capability delegation within programs:**

Capabilities are first-class values. A program that receives a `DirCap` via `$data` can pass it to functions and to `narrow` for attenuation:

```tinct
# Narrow %data to a subdirectory and pass the narrower cap to a helper
[safe-cap: [narrow %data "configs"]]
[read-config safe-cap "app.yaml"]
```

`narrow` returns a new `DirCap` rooted at the subdirectory — the helper can only open files under `data/configs/`, not anywhere else in `/var/data`.

The following sections describe the sandboxing layers in detail.

### Filesystem Sandbox (cap-std + Landlock)

Filesystem access is controlled via the object capability model: `$include` requires a DirCap as its first argument. DirCaps are created via `--cap-fs NAME=PATH` flags or injected automatically as `%pwd` and `%libdir`. Each DirCap is backed by cap-std's RESOLVE_BENEATH enforcement, which confines all file access to the cap's root directory at the OS level.

**cap-std RESOLVE_BENEATH:** Primary enforcement. Every DirCap wraps a `cap_std::fs::Dir` that confines all file operations (open, canonicalize, etc.) to its root directory. Absolute paths are rejected. Symlinks and `../` traversal are resolved, but the final path must remain within the root. This is path-based confinement at the syscall level — works on all platforms.

**Landlock (Linux 5.13+):** Auto-triggered when `--cap-fs` entries are present (unless `--no-landlock` is set). Landlock is a kernel-level LSM that enforces read-only access on the `--cap-fs` paths plus the directories containing the main input files. Defense-in-depth: if a bug in cap-std or DirCap handling allows an unauthorized path to reach `open()`, Landlock catches it at the kernel level. Gracefully degrades on older kernels (silently skipped).

**Sandboxing model:**
- Every file access requires a DirCap. No ambient `$include "path.llt"` — must be `[include %pwd "path.llt"]` or `[include %libdir "io.llt"]`.
- `%pwd` and `%libdir` are injected automatically (suppress with `--no-pwd` / `--no-libdir`).
- `--cap-fs data=/var/data` injects `%data` as a DirCap for `/var/data`. Repeatable.
- `--no-fs` disables all filesystem access — `$include` returns an error immediately, bypassing cap checks.
- Stdlib is embedded via `include_str!` at compile time — no filesystem access, unaffected by sandboxing.
- REPL: `%pwd` defaults to cwd. LSP: `%pwd` defaults to workspace root (or document directory if no workspace).

**Check ordering in `$include`:** cap-std RESOLVE_BENEATH → cache lookup → cycle detection → read file → **hash check (if hash provided)** → cache store → parse. Cache lookup and cycle detection are cheap in-memory operations; the read and hash are deferred until after both pass. On a cache hit, the stored hash map (recorded on first read) is checked against the caller's expected algorithm and digest; if they match, the cached result is returned without re-reading. The cache is session-scoped (in-memory only; not persisted to disk).

### Import Integrity Hashes

`$include` accepts an optional integrity hash as a second argument. When present, tinct verifies the hash of the raw file bytes before parsing. A mismatch is a hard error — evaluation does not proceed.

```tinct
# Without hash: normal include (no integrity check)
[include "config/settings.llt"]

# With hash: content is verified before evaluation
[include "config/settings.llt" "blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc5a0f67f7df2f8e"]
```

The hash is a quoted string with the format `"algo:hexdigest"`. The algorithm name and hex digest are separated by `:`.

**Default algorithm: BLAKE3.** BLAKE3 (O'Connor et al. 2020) is the default and preferred algorithm. Against quantum adversaries, Grover's algorithm halves the bit-security of any hash function. BLAKE3 outputs 256 bits, giving 128 bits of quantum security — well above the threshold considered infeasible even with near-term quantum hardware. BLAKE3 is also significantly faster than SHA-2 or SHA-3, though for typical config files (< 1 MB) this is imperceptible.

**Currently supported algorithm:** Only BLAKE3 is supported. The hash prefix determines the algorithm:

| Prefix | Algorithm | Hex length | Quantum security |
|--------|-----------|-----------|-----------------|
| `blake3:` | BLAKE3 | 64 chars (256 bits) | 128 bits |

Additional algorithms (SHA3-256, SHA3-512, SHA-256) may be added in the future for interoperability. `tinct hash` outputs BLAKE3.

The hex digest must be exactly the correct length for the algorithm. Shorter or longer strings are rejected with a clear error before any file access.

**What is hashed:** Raw file bytes (`std::fs::read` → `Vec<u8>`), before UTF-8 validation or parsing. No normalization. Independently verifiable: `b3sum file.llt`, `sha3-256sum file.llt`, `sha256sum file.llt`.

**Why raw bytes and not semantic content:** A semantic hash would require evaluating the include before verifying it — circular, since evaluation IS the import. Tinct also has no canonical normal form (general recursion means normalization does not always terminate). Raw bytes are simpler, stable, and independently verifiable with standard tools.

**Generating the hash:** The `tinct hash <file>` subcommand outputs the hash in the correct format:

```bash
$ tinct hash config/settings.llt
blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc5a0f67f7df2f8e
```

(The example digest above is the BLAKE3 hash of the empty string — real files produce different values.)

Use the output as the second argument to `[include ...]`:

```tinct
[include "config/settings.llt" "blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc5a0f67f7df2f8e"]
```

**Cache integration:** When a file is first included with a hash, `builtin_include` reads the raw bytes (`Vec<u8>`), computes the hash, verifies it matches the expected value, and stores `{evaluated_result, hash_map: HashMap<Algo, HexDigest>}` in the session cache keyed by canonical path. On subsequent includes of the same path:

- If the new include provides a hash: look up the algorithm in `hash_map`. Hit → compare; match returns cached result, mismatch errors. Miss (algorithm not seen before) → re-read the file, compute the new algorithm's hash, verify, store the new entry in `hash_map`, return the same cached evaluated result. This ensures integrity is always verified against fresh bytes for each new algorithm, while the evaluated result is reused (files are assumed not to change during a single `tinct run` session).
- If the new include has no hash: return cached result without hash verification (same as today).

The cache is session-scoped and held in memory — it does not persist across `tinct run` invocations. Stdlib is embedded via `include_str!` at compile time and is not subject to hash verification.

**Error on mismatch:**

```
include: hash mismatch for 'config/settings.llt'
  expected: blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc5a0f67f7df2f8e
  actual:   blake3:b7c2f3a1d9e891f42c2d4b578c9a0e3f1b6d7e8a2c5f0d4e9b3a7c1f8e2d5b4
```

**Conflicting hashes:** If the same file is included twice with different expected hashes for the same algorithm, the second include errors — the hash verified on the first read is stored in `hash_map`; if the second caller's expected hash differs from it, the mismatch error fires without re-reading the file.

**Require-integrity mode:** `--require-integrity` makes any `[include ...]` without a hash a hard error. Use for environments where all dependencies must be content-addressed:

```bash
tinct run --require-integrity --cap-fs vendor=./vendor main.llt
```

Note: `--no-fs` disables `$include` entirely, making `--require-integrity` redundant. `--require-integrity` is meaningful when DirCaps are available (`%pwd`, `%libdir`, `--cap-fs`), requiring all `$include` calls to carry a hash.

**Use cases:** Pinning a shared config file in CI so an unreviewed change fails loudly. Verifying third-party tinct libraries. High-security evaluation environments where all includes must be content-addressed.

**Builtin change:** `builtin_include` gains an optional second positional argument — the hash string. No grammar or parser changes. All existing `[include "path"]` calls without a hash continue to work unchanged.

### Network Sandbox (seccomp-bpf)

Network syscalls are controlled by the `--cap-net` flag and the NetCap allowlist.

- Default: network blocked. seccomp-bpf blocks `socket`, `connect`, `bind`, `listen`, `accept` syscalls. Even if a vulnerability allows code injection, the process cannot make network connections.
- Network syscalls are allowed automatically when any `--cap-net NAME=ENTRY` flag is present — the presence of a network capability implies network authority. There is no separate `--allow-network` flag.
- `--cap-net` entries define the NetCap allowlist (host:port, glob patterns, CIDR ranges). The NetCap check runs before socket creation. Only connections to allowed hosts/ports succeed.
- Seccomp filter installed in `run_eval()` after Landlock, before evaluation starts (process-level, not per-eval).
- Linux-only; on other platforms, network features are controlled at the application level. Logs a warning on non-Linux.

### Resource Sandbox (rlimit)

Prevents evaluation from consuming unbounded resources (DoS protection, runaway recursion). Uses POSIX `setrlimit` — works on Linux, macOS, and BSDs.

| Limit | Default | CLI Override | Applies to |
|-------|---------|-------------|------------|
| `RLIMIT_AS` | 512MB | `--max-memory 1G` | All subcommands |
| `RLIMIT_CPU` | 30s | `--max-cpu 60` | `eval` only |
| Wall-clock | none | `--timeout <duration>` | `eval` only |
| `RLIMIT_NOFILE` | 64 | `--max-fds 128` | All subcommands |

`RLIMIT_CPU` and `--timeout` apply only to `eval`. `RLIMIT_CPU` measures CPU time (time the process spends on-CPU); `--timeout` measures wall-clock time (elapsed real time). For adversarial inputs where the distinction matters (e.g. a program that sleeps or performs many syscalls), use `--timeout`. Both limits coexist — whichever fires first terminates evaluation.

`--timeout` is specified as a duration string: `30s`, `500ms`, `2m`. Implemented via `alarm(2)` + SIGALRM. When the timeout fires, the process exits with code 2. The `lsp` and `repl` subcommands are long-lived processes where cumulative CPU time is expected — a 30-second CPU cap would kill them during normal use. Memory and file descriptor limits still apply to all subcommands as safety nets.

### Process Sandbox (seccomp-bpf)

Tinct is a pure configuration language — it should never spawn child processes.

- Always on. Blocks `fork`, `execve`, `execveat` via seccomp. `clone` is allowed because Tinct uses worker threads (64MB stack for evaluator deep recursion workaround).
- No CLI flag to disable — there is no legitimate reason for a config evaluator to fork or exec.
- Linux-only; on other platforms, Tinct simply never calls process-creation APIs. Logs a warning on non-Linux.

### Initialization Order

Sandbox setup in `run_eval()` follows this sequence:

1. Parse CLI (clap) — get `--cap-fs`, `--max-memory`, etc.
2. Set up timeout (SIGALRM)
3. Set up rlimit (resource caps: RLIMIT_AS, RLIMIT_CPU, RLIMIT_NOFILE)
4. Read stdin (JSON auto-detection)
5. Load stdlib (`create_stdlib_env()` — uses `include_str!`, no filesystem access)
6. Set up Landlock (filesystem ACLs from `--cap-fs` paths, auto-triggered)
7. Set up seccomp-bpf (network block, process block)
8. Inject capabilities (`%pwd`, `%stdin`, `%libdir`, `--cap-fs`, `--cap-net`)
9. Dispatch evaluation

Landlock and seccomp are applied after stdlib loading (stdlib uses `include_str!` at compile time, so no filesystem access is needed). `prctl(PR_SET_NO_NEW_PRIVS)` is called before seccomp installation.

### Platform Support

| Sandbox | Linux | macOS | Windows |
|---------|-------|-------|---------|
| Filesystem (application) | Yes | Yes | Yes |
| Filesystem (Landlock) | 5.13+ | No | No |
| Network (seccomp) | 3.5+ | No | No |
| Resources (rlimit) | Yes | Yes | No |
| Process (seccomp) | 3.5+ | No | No |

On non-Linux platforms, the application-level filesystem check and rlimit (where available) provide the core security guarantees. seccomp and Landlock are defense-in-depth layers specific to Linux. When unavailable, a warning is logged and the application-level checks remain the sole barrier.

### EvalConfig Integration

The filesystem allowlist lives in `EvalConfig` (immutable per evaluation session). For the full `EvalConfig`/`EvalState` specification, see [Architecture](16-architecture.md) §EvalContext.

```rust
struct EvalConfig {
    base_dir: cap_std::fs::Dir,    // DirCap for relative path resolution
    stdlib_env: Rc<RefCell<Environment>>,
    no_fs: bool,
    require_integrity: bool,
}
```

`$include` checks `config.allowed_paths` before reading. Landlock, seccomp, and rlimit are set up in `main()` before evaluation starts — they are process-level restrictions, not per-eval.

### Adversarial Evaluation

For services that evaluate attacker-controlled tinct programs (playgrounds, API endpoints, CTF infrastructure), `tinct run` is designed to be used as a sandboxed child process. The calling service is the parent — it spawns `tinct run` with the appropriate flags, captures stdout/stderr, and inspects the exit code. `tinct run` is one-shot per request; the caller handles concurrency by spawning multiple child processes.

**Flags for adversarial use:**

```bash
tinct run --no-fs --timeout 5s --max-memory 64M --max-cpu 10 main.llt
```

| Flag | Effect |
|------|--------|
| `--no-fs` | Disables `$include` entirely (empty filesystem allowlist) |
| `--timeout <dur>` | Wall-clock limit (e.g. `5s`, `500ms`); exit code 2 on expiry |
| `--max-memory <size>` | Address space limit (e.g. `64M`, `512M`) |
| `--max-cpu <secs>` | CPU time limit in seconds |

**Exit codes:**

| Code | Meaning |
|------|---------|
| 0 | Success — output on stdout |
| 1 | Eval/parse/type error — error message on stderr |
| 2 | Timeout — wall-clock limit exceeded (`--timeout`) |
| 3 | Resource limit — memory or CPU cap hit (SIGXCPU/SIGXFSZ) |

**Architecture:** `tinct run` is the sandboxed process. The parent service uses the exit code to distinguish timeout (code 2) from hard resource exhaustion (code 3) from user errors (code 1). All four sandboxing layers (filesystem allowlist, network seccomp, rlimit, process seccomp) compose — `--no-fs --timeout 5s --max-memory 64M` enables all simultaneously.

**Security note:** The `IncludeForbidden` error raised by `$include` in `--no-fs` mode is catchable via `$try` (intentional, following the Nix `tryEval` model for graceful degradation). An attacker can detect `--no-fs` mode by wrapping `$include` in `$try`. This is accepted because making the error uncatchable would prevent legitimate programs from falling back to embedded defaults when external config files are unavailable. See doc/10-errors.md §Special error properties for the full rationale.

**Comparison with VM-level isolation (e.g. Cloudflare Workers / V8 isolates):** V8 isolates achieve language-level sandboxing with microsecond startup time and planet-scale density, at the cost of tying the sandbox to a specific JavaScript engine. `tinct run` uses OS-level process isolation — a stronger security boundary (separate address space, separate file descriptor table, kernel enforcement via seccomp and Landlock) at the cost of per-process overhead (~10ms fork+exec). For tinct's scale (configuration evaluation, not request-per-millisecond hot path), OS process isolation is the correct tradeoff.

### Rust Crates

- `landlock` — official Landlock LSM wrapper
- `seccompiler` — seccomp-bpf filter builder (from rust-vmm/Firecracker)
- `rlimit` — setrlimit wrapper
