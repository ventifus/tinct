# What If: Directory Capability Permissions for tinct

**State:** Accepted — 2026-05-11

What would it take to give `DirCap` a fine-grained permission model — making the
principle of least authority enforceable at the CLI grant boundary, not just inside
scripts via `narrow`?

## Current State

`DirCap` is all-or-nothing. `--cap-fs root=.` grants full read and write access to
the workspace. A script like `scripts/docgen.llt` only needs to read files, but
receives the same authority as a script that writes output. There is no way to
express "read-only directory access" at the grant boundary.

`--cap-file`, by contrast, already uses a `NAME=PATH:MODE` format with `r`, `rb`,
`w`, `wb` modes that produce typed `Handle@[Readable]` or `Handle@[Writable]` values.
`DirCap` should be consistent.

## Design

### Capability Flags

Seven orthogonal capabilities apply to a directory tree:

| Flag | POSIX analogue | What it enables |
|------|---------------|-----------------|
| `Readable` | `r` on files | Open files for reading: `open`, `slurp`, `lines` |
| `Statable` | `r` on dirs / `stat(2)` | Read file metadata: name, type, size, mtime via `list-dir`; does NOT require reading file content |
| `Listable` | `r` on dirs | Enumerate directory entries: `list-dir`; implies `Statable` |
| `Writable` | `w` on files + dir | Write/overwrite files and create new files: `open "w"`, `write-file`, `write-file-atomic` |
| `Appendable` | `a` flag | Open files in append mode: no read, no truncate |
| `Deletable` | `w` + `x` on dir | Remove files and directories: `delete-file` |
| `Renameable` | `w` + `x` on dir | Rename or move files within the tree: `rename-file` |

Flags are purely additive — specifying one never removes another. `Statable` and
`Listable` are distinct: `Listable` implies `Statable` (listing entries returns
metadata), but `Statable` alone allows `stat`-style queries on known paths without
enumeration. `Creatable` from earlier designs is merged into `Writable` (in POSIX,
directory write permission covers both creating new files and modifying existing ones).

### CLI Syntax

Extends `--cap-fs` with a `:MODE` suffix, aligned with `--cap-file NAME=PATH:MODE`.
Path and mode are split on the **last** `:` via `rsplit_once` — identical to `--cap-file`
parsing, which handles Windows drive letters correctly.

**Shorthand mode letters** — each letter implies a sensible bundle of flags:

```
--cap-fs NAME=PATH[:MODE]
```

| Letter | Flags granted | Rationale |
|--------|--------------|-----------|
| `r` | `Readable Listable Statable` | Reading implies listing and stat-ing — you need to know what's there |
| `w` | `Writable Appendable Deletable Renameable` | All mutation: overwrite, append, delete, rename — all are "write authority" |
| `a` | `Appendable` | Strict append-only; no overwrite, no delete, no rename, no reads |
| `s` | `Statable` | Metadata queries only — no content reads, no listing |
| `l` | `Listable Statable` | Directory traversal — enumerate entries and their metadata, no file reads |

Letters compose by union: `rw` = `r` ∪ `w` = `{Readable, Listable, Statable, Writable, Appendable, Deletable, Renameable}`.
`a` alone is useful only when you need strictly append-without-overwrite (e.g. an audit log that must not be truncated).
No mode = `:rw` for backward compatibility.

```bash
--cap-fs root=.:r         # read files, list dirs, stat metadata
--cap-fs out=./build:w    # write/create/delete/rename — no reads
--cap-fs log=/var/log:a   # append-only log directory
--cap-fs src=./src:l      # walk tree and inspect metadata, no file reads
--cap-fs cache=.:rw       # full read-write (explicit, same as default)
```

**Extended syntax** — for cases where the shorthand bundles are too coarse, specify
a tinct-style list of capability names directly. No assumptions are made: only the
listed capabilities are granted.

```bash
# Read file content + stat, but no directory listing
--cap-fs data='./data:[Readable Statable]'

# Write and append, but no delete or rename
--cap-fs scratch='./tmp:[Writable Appendable]'

# Stat-only on a known path (existence/freshness checks, no enumeration)
--cap-fs marker='./deploy:[Statable]'
```

The extended form is detected by the mode value starting with `[`. The content is
parsed as a whitespace-separated list of capability names (no commas, no quotes —
same as tinct identifier lists). Unknown names are a startup error.

No binary distinction exists for `DirCap` (unlike `--cap-file`'s `:rb`/`:wb`) —
binary vs. text is a property of the opened `Handle`, not of the directory grant.

### Type System

`DirCap` gains a capability row parameter, mirroring `Handle`:

```tinct
--- caps: [%root:  @[DirCap [Readable Listable Statable]]]       # :r
--- caps: [%out:   @[DirCap [Writable Deletable Renameable]]]   # :w
--- caps: [%log:   @[DirCap [Appendable]]]                      # :a
--- caps: [%data:  @[DirCap [Readable Statable]]]               # :[Readable Statable]
```

`@DirCap` without flags in caps declarations is treated as
`@[DirCap [Readable Listable Statable Writable Deletable Renameable Appendable]]`
(full access) during a backward-compat transition period.

**Row-polymorphic builtin signatures:**

```tinct
open      [cap@[DirCap [Readable ...]]   path@String "r"] → Handle@[Readable ...]
open      [cap@[DirCap [Writable ...]]   path@String "w"] → Handle@[Writable ...]
open      [cap@[DirCap [Appendable ...]] path@String "a"] → Handle@[Appendable ...]
list-dir  [cap@[DirCap [Listable ...]]   path@String]     → [Seq Dict]
stat-file [cap@[DirCap [Statable ...]]   path@String]     → Dict               # future builtin
write-file        [cap@[DirCap [Writable ...]]   path@String content@String]
write-file-atomic [cap@[DirCap [Writable ...]]   path@String content@String]
delete-file        [cap@[DirCap [Deletable ...]]  path@String]
rename-file        [cap@[DirCap [Renameable ...]] old@String  new@String]
```

The `...` row tail (same as in record types — `[name: String ...]`) means "DirCap
with at least this capability flag, plus possibly others." A caller passing
`[DirCap [Readable Listable Writable]]` to `write-file` satisfies `[DirCap [Writable ...]]`
because `Writable` is present and `...` absorbs `Readable Listable`. Without the
tail, `[DirCap [Writable]]` would be an exact type that rejects any cap holding
additional flags. The concrete syntax for named row tails (`...r`) follows the same
convention as record row variables.

### `narrow` for In-Script Attenuation

A script can further restrict a DirCap it receives before passing it to untrusted
code. `narrow` produces a DirCap with a subset of the original flags:

```tinct
# Received: %dir@[DirCap [Readable Listable Statable Writable Deletable Renameable]]
# Pass read-only to untrusted helper
[helper [narrow %dir Readable Listable Statable]]

# Pass write-only to output function
[write-output [narrow %dir Writable]]

# Restrict to a subdirectory (Subtree attenuation)
[scan [narrow %root Subtree "src/lib"]]
```

`narrow` with flags not present in the source cap is a runtime error ("cannot
amplify capability"). `Subtree` is a separate attenuation axis: it restricts the
path root without changing which operation flags are held.

### `%pwd` Default Permissions

`%pwd` is injected as `[DirCap [Readable Listable Statable Writable Deletable Renameable Appendable]]`
— full access, same effective authority as today, now tracked in the type system.

## What Would Change

### `src/main.rs` — CLI parsing

**`--cap-fs`:** Parse optional `:MODE` suffix by splitting on the last `:` via
`rsplit_once`. No `:` → full access (`DirPerms::full()`). If mode starts with `[`,
parse as extended capability list (`[Readable Statable Writable]`). Otherwise parse
as letter sequence (each letter adds its bundle). Unknown names or letters are a
startup error.

**`--cap-file`:** Same extension — the extended `:[Cap1 Cap2 ...]` syntax is also
accepted, with valid names `Readable`, `Writable`, `Appendable`, `Binary`. No
`:mode` suffix → open file read-write (full access). Existing `r`/`rb`/`w`/`wb`
letter shorthands remain valid (backward compat).

### `src/value.rs` — `DirPerms` and `Value::DirCap`

```rust
pub struct DirPerms {
    pub readable:   bool,
    pub statable:   bool,
    pub listable:   bool,
    pub writable:   bool,   // covers create + overwrite
    pub appendable: bool,
    pub deletable:  bool,
    pub renameable: bool,
}

impl DirPerms {
    pub fn full() -> Self { /* all true */ }
    pub fn from_letter(c: char) -> Self { /* r/w/a/s/l bundle */ }
}
```

`Value::DirCap` and `Value::RevocableDirCap` gain a `perms: DirPerms` field.
All existing construction sites use `DirPerms::full()`.

### `src/builtins_io.rs` — permission checks

Each DirCap-consuming builtin checks the relevant flag; emits a capability error on
violation (`"DirCap: operation requires <Flag> permission"`):

- `builtin_open`: `readable`/`writable`/`appendable` per open mode
- `builtin_list_dir`: `listable`
- `builtin_write` / `builtin_write_atomic`: `writable`
- `builtin_delete_file`: `deletable`
- `builtin_rename_file`: `renameable`

### `src/type_env.rs` — type registration

`%pwd`, `%libdir`, and all `--cap-fs` injections gain appropriate row types.
Builtin signatures updated to use row-polymorphic `[DirCap [Flag ...]]` constraints.

## Prerequisites

The row-polymorphic capability row type for `Handle` is already established. `DirCap`
reuses the same machinery. `delete-file` and `rename-file` are not yet implemented
as builtins; `Deletable` and `Renameable` can be registered in the type system now
with placeholder runtime errors.

## References

- Miller, M.S. (2006). *Robust Composition.* PhD thesis, Johns Hopkins University.
  — [principle of least authority; capability attenuation via narrowing]
- Dennis, J.B. & Van Horn, E.C. (1966). "Programming semantics for multiprogrammed
  computations." *CACM 9(3).* — [file descriptors as capabilities]
- Saltzer, J.H. & Schroeder, M.D. (1975). "The Protection of Information in
  Computer Systems." *Proc. IEEE 63(9).* — [principle of least privilege]
- POSIX.1-2017. §3.164 "File access permissions." — [`r`/`w`/`x` bit semantics
  that DirCap flags map onto]
