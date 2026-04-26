# Tooling

## Formatter (`llt fmt`)

**Zero-configuration** code formatter for Tinct files. Operates on the hand-written lexer's token stream (not the AST), so comments and whitespace are preserved and reformatted.

**Architecture:** The formatter lexes source into a token stream (including comment tokens), groups tokens into bracket-delimited blocks, applies formatting rules, and emits reformatted source. It does not parse to AST — this avoids losing comments (pest silently drops them) and avoids a dependency on the iterative parser.

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

### Configurability: Zero-Config

No formatting options. The formatter defines the canonical Tinct style. The only CLI flags control I/O behavior:
- `--check` — exit 1 if any file is not formatted (CI mode)
- `--in-place` — overwrite files in place
- `--stdin` — read from stdin, write to stdout

**Rationale:** gofmt's zero-config philosophy. One canonical style eliminates bikeshedding. Pre-1.0, if a genuine need for configurability emerges, knobs can be added later. Starting opinionated is easier than tightening.

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

## Sandboxing & Security

Tinct uses four unprivileged sandboxing layers to restrict what evaluation can access. All work without root privileges. Sandbox flags are scoped to the subcommands that use them — for example, `--no-fs` and `--timeout` are `eval` subcommand flags.

### Filesystem Sandbox (Application-Level + Landlock)

Two layers of defense for `$include` and any future file I/O:

**Application-level allowlist:** `$include` checks resolved paths against an allowlist before reading. All paths are canonicalized first (resolving symlinks and `../` traversal), then checked using path-ancestor matching: `canonical.ancestors().any(|a| allowed_paths.contains(a))`. This is path-element-based, not substring-based — `/tmp/allowed` does not match `/tmp/allowed2`. This is the primary control — works on all platforms, produces clear error messages.

**Landlock (Linux 5.13+):** Kernel-enforced filesystem ACLs as defense-in-depth. If a bug bypasses the application-level check, Landlock catches it at the kernel level. Detected at runtime; gracefully degrades on older kernels or non-Linux platforms (logs a warning, falls back to application-level check as the sole barrier).

**Allowlist model:**
- `--allow-path <dir>` adds a directory tree to the allowlist. Repeatable. Global flag.
- Default: `--allow-path .` (current working directory subtree). Project files accessible, nothing else.
- `--allow-path /` disables filesystem sandboxing entirely.
- `--no-fs` sets the allowlist to empty — `$include` is fully disabled. Use for adversarial inputs where no filesystem access should be possible.
- Absolute paths in `$include` are allowed if they resolve within any allowed path.
- Symlinks: canonicalize to real path, then check. Symlinks pointing outside all allowed paths fail.
- `--allow-path` values are themselves canonicalized at CLI parse time (once), not on every include check.
- Stdlib is embedded via `include_str!` at compile time — no filesystem access, unaffected by sandboxing.
- REPL: default allow-path is cwd. LSP: workspace root (or document directory if no workspace).

**Check ordering in `$include`:** canonicalize path → allowlist check → cache lookup → cycle detection → read file → **hash check (if hash provided)** → cache store → parse. Cache lookup and cycle detection are cheap in-memory operations; the read and hash are deferred until after both pass. On a cache hit, the stored hash map (recorded on first read) is checked against the caller's expected algorithm and digest; if they match, the cached result is returned without re-reading. The cache is session-scoped (in-memory only; not persisted to disk).

**Error message format:** `"include: path '/etc/passwd' is outside allowed paths (allowed: ['/home/user/project'])"` — shows resolved path and the allowlist so the user knows exactly what happened and how to fix it.

```bash
tinct --allow-path . eval main.llt                           # default (explicit)
tinct --allow-path ./lib --allow-path /shared eval main.llt  # explicit allowlist
tinct --allow-path / eval main.llt                           # unrestricted
```

### Import Integrity Hashes

`$include` accepts an optional integrity hash as a second argument. When present, tinct verifies the hash of the raw file bytes before parsing. A mismatch is a hard error — evaluation does not proceed.

```tinct
# Without hash: normal include (no integrity check)
[call $include "config/settings.llt"]

# With hash: content is verified before evaluation
[call $include "config/settings.llt" "blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc5a0f67f7df2f8e"]
```

The hash is a quoted string with the format `"algo:hexdigest"`. The algorithm name and hex digest are separated by `:`.

**Default algorithm: BLAKE3.** BLAKE3 (O'Connor et al. 2020) is the default and preferred algorithm. Against quantum adversaries, Grover's algorithm halves the bit-security of any hash function. BLAKE3 outputs 256 bits, giving 128 bits of quantum security — well above the threshold considered infeasible even with near-term quantum hardware. BLAKE3 is also significantly faster than SHA-2 or SHA-3, though for typical config files (< 1 MB) this is imperceptible.

**Multiple algorithms supported:** The hash prefix determines the algorithm. Supported algorithms:

| Prefix | Algorithm | Hex length | Quantum security |
|--------|-----------|-----------|-----------------|
| `blake3:` | BLAKE3 | 64 chars (256 bits) | 128 bits |
| `sha3-256:` | SHA3-256 (FIPS 202) | 64 chars (256 bits) | 128 bits † |
| `sha3-512:` | SHA3-512 (FIPS 202) | 128 chars (512 bits) | 256 bits † |
| `sha256:` | SHA-256 | 64 chars (256 bits) | ~85 bits (Grover) |

† SHA-3's sponge construction gives capacity-based quantum security bounds that differ from the simple "output-bits ÷ 2" rule. SHA3-256 has capacity 512, giving 256 bits of classical collision resistance and 128 bits against Grover; SHA3-512 has capacity 1024, giving 256 bits of quantum security. The bounds are sound but derive from different assumptions than SHA-2.

`sha256:` is accepted for interoperability but not recommended for new programs — Grover's algorithm reduces its effective collision resistance to ~85 bits. `llt hash` defaults to BLAKE3.

The hex digest must be exactly the correct length for the algorithm. Shorter or longer strings are rejected with a clear error before any file access.

**What is hashed:** Raw file bytes (`std::fs::read` → `Vec<u8>`), before UTF-8 validation or parsing. No normalization. Independently verifiable: `b3sum file.llt`, `sha3-256sum file.llt`, `sha256sum file.llt`.

**Why raw bytes and not semantic content:** A semantic hash would require evaluating the include before verifying it — circular, since evaluation IS the import. Tinct also has no canonical normal form (general recursion means normalization does not always terminate). Raw bytes are simpler, stable, and independently verifiable with standard tools.

**Generating the hash:** The `llt hash <file>` subcommand outputs the hash in the correct format:

```bash
$ llt hash config/settings.llt
blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc5a0f67f7df2f8e

$ llt hash --algo sha3-256 config/settings.llt
sha3-256:a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a
```

(The example digests above are the BLAKE3 and SHA3-256 hashes of the empty string — real files produce different values.)

Use the output as the second argument to `[call $include ...]`:

```tinct
[call $include "config/settings.llt" "blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc5a0f67f7df2f8e"]
```

**Cache integration:** When a file is first included with a hash, `builtin_include` reads the raw bytes (`Vec<u8>`), computes the hash, verifies it matches the expected value, and stores `{evaluated_result, hash_map: HashMap<Algo, HexDigest>}` in the session cache keyed by canonical path. On subsequent includes of the same path:

- If the new include provides a hash: look up the algorithm in `hash_map`. Hit → compare; match returns cached result, mismatch errors. Miss (algorithm not seen before) → re-read the file, compute the new algorithm's hash, verify, store the new entry in `hash_map`, return the same cached evaluated result. This ensures integrity is always verified against fresh bytes for each new algorithm, while the evaluated result is reused (files are assumed not to change during a single `llt eval` session).
- If the new include has no hash: return cached result without hash verification (same as today).

The cache is session-scoped and held in memory — it does not persist across `llt eval` invocations. Stdlib is embedded via `include_str!` at compile time and is not subject to hash verification.

**Error on mismatch:**

```
include: hash mismatch for 'config/settings.llt'
  expected: blake3:af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc5a0f67f7df2f8e
  actual:   blake3:b7c2f3a1d9e891f42c2d4b578c9a0e3f1b6d7e8a2c5f0d4e9b3a7c1f8e2d5b4
```

**Conflicting hashes:** If the same file is included twice with different expected hashes for the same algorithm, the second include errors — the hash verified on the first read is stored in `hash_map`; if the second caller's expected hash differs from it, the mismatch error fires without re-reading the file.

**Require-integrity mode:** `--require-integrity` makes any `[call $include ...]` without a hash a hard error. Use for environments where all dependencies must be content-addressed:

```bash
llt eval --require-integrity --allow-path ./vendor main.llt
```

Note: `--no-fs` disables `$include` entirely, making `--require-integrity` redundant. `--require-integrity` is meaningful alongside `--allow-path`, where `$include` is permitted but must always carry a hash.

**Use cases:** Pinning a shared config file in CI so an unreviewed change fails loudly. Verifying third-party tinct libraries. High-security evaluation environments where all includes must be content-addressed.

**Builtin change:** `builtin_include` gains an optional second positional argument — the hash string. No grammar or parser changes. All existing `[call $include "path"]` calls without a hash continue to work unchanged.

### Network Sandbox (seccomp-bpf)

No network features exist yet, but the sandbox is designed so future features (`$fetch`, remote includes) have a security model ready.

- Default: network blocked. seccomp-bpf blocks `socket`, `connect`, `bind`, `listen`, `accept` syscalls. Even if a future vulnerability allows code injection, the process cannot make network connections.
- `--allow-network` lifts the restriction (for future network features). Global flag.
- `--allow-host <host:port>` for fine-grained control (future — requires application-level checking since seccomp cannot filter by host).
- Seccomp filter installed in `main()` before evaluation starts (process-level, not per-eval).
- Linux-only; on other platforms, network features are controlled at the application level. Logs a warning on non-Linux.

### Resource Sandbox (rlimit)

Prevents evaluation from consuming unbounded resources (DoS protection, runaway recursion). Uses POSIX `setrlimit` — works on Linux, macOS, and BSDs.

| Limit | Default | CLI Override | Applies to |
|-------|---------|-------------|------------|
| `RLIMIT_AS` | 512MB | `--max-memory 1G` | All subcommands |
| `RLIMIT_CPU` | 30s | `--max-cpu 60` | `eval` only |
| Wall-clock | none | `--timeout <duration>` | `eval` only |
| `RLIMIT_NOFILE` | 64 | `--max-fds 128` | All subcommands |
| `RLIMIT_FSIZE` | 10MB | — | All subcommands |

`RLIMIT_CPU` and `--timeout` apply only to `eval`. `RLIMIT_CPU` measures CPU time (time the process spends on-CPU); `--timeout` measures wall-clock time (elapsed real time). For adversarial inputs where the distinction matters (e.g. a program that sleeps or performs many syscalls), use `--timeout`. Both limits coexist — whichever fires first terminates evaluation.

`--timeout` is specified as a duration string: `30s`, `500ms`, `2m`. Implemented via `alarm(2)` + SIGALRM. When the timeout fires, the process exits with code 2. The `lsp` and `repl` subcommands are long-lived processes where cumulative CPU time is expected — a 30-second CPU cap would kill them during normal use. Memory and file descriptor limits still apply to all subcommands as safety nets.

### Process Sandbox (seccomp-bpf)

Tinct is a pure configuration language — it should never spawn child processes.

- Always on. Blocks `fork`, `execve`, `execveat` via seccomp. `clone` is allowed because Tinct uses worker threads (64MB stack for pest deep nesting workaround).
- No CLI flag to disable — there is no legitimate reason for a config evaluator to fork or exec.
- Linux-only; on other platforms, Tinct simply never calls process-creation APIs. Logs a warning on non-Linux.

### Initialization Order

Sandbox setup in `main()` follows this sequence:

1. Parse CLI (clap) — get `--allow-path`, `--max-memory`, etc.
2. Set up rlimit (resource caps)
3. Set up seccomp-bpf (network block, process block)
4. Set up Landlock (filesystem ACLs from `--allow-path`)
5. Load stdlib (`create_stdlib_env()` — uses `include_str!`, no filesystem access)
6. Dispatch subcommand (eval/repl/lsp)

Seccomp and Landlock are applied before any evaluation. `prctl(PR_SET_NO_NEW_PRIVS)` is called before seccomp installation.

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

The filesystem allowlist lives in `EvalConfig` (immutable per evaluation session):

```rust
struct EvalConfig {
    base_dir: PathBuf,
    stdlib_env: Rc<RefCell<Environment>>,
    allowed_paths: Vec<PathBuf>,    // canonicalized at CLI parse time
    // future: allowed_hosts: Vec<String>,
}
```

`$include` checks `config.allowed_paths` before reading. Landlock, seccomp, and rlimit are set up in `main()` before evaluation starts — they are process-level restrictions, not per-eval.

### Adversarial Evaluation

For services that evaluate attacker-controlled tinct programs (playgrounds, API endpoints, CTF infrastructure), `llt eval` is designed to be used as a sandboxed child process. The calling service is the parent — it spawns `llt eval` with the appropriate flags, captures stdout/stderr, and inspects the exit code. `llt eval` is one-shot per request; the caller handles concurrency by spawning multiple child processes.

**Flags for adversarial use:**

```bash
llt eval --no-fs --timeout 5s --max-memory 64M --max-cpu 10 main.llt
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

**Architecture:** `llt eval` is the sandboxed process. The parent service uses the exit code to distinguish timeout (code 2) from hard resource exhaustion (code 3) from user errors (code 1). All four sandboxing layers (filesystem allowlist, network seccomp, rlimit, process seccomp) compose — `--no-fs --timeout 5s --max-memory 64M` enables all simultaneously.

**Security note:** The `IncludeForbidden` error raised by `$include` in `--no-fs` mode is catchable via `$try` (intentional, following the Nix `tryEval` model for graceful degradation). An attacker can detect `--no-fs` mode by wrapping `$include` in `$try`. This is accepted because making the error uncatchable would prevent legitimate programs from falling back to embedded defaults when external config files are unavailable. See doc/10-errors.md §Special error properties for the full rationale.

**Comparison with VM-level isolation (e.g. Cloudflare Workers / V8 isolates):** V8 isolates achieve language-level sandboxing with microsecond startup time and planet-scale density, at the cost of tying the sandbox to a specific JavaScript engine. `llt eval` uses OS-level process isolation — a stronger security boundary (separate address space, separate file descriptor table, kernel enforcement via seccomp and Landlock) at the cost of per-process overhead (~10ms fork+exec). For tinct's scale (configuration evaluation, not request-per-millisecond hot path), OS process isolation is the correct tradeoff.

### Rust Crates

- `landlock` — official Landlock LSM wrapper
- `seccompiler` — seccomp-bpf filter builder (from rust-vmm/Firecracker)
- `rlimit` — setrlimit wrapper
