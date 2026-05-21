# What If: Rust Primitive Privacy via Virtual Modules for tinct

**State:** Accepted — 2026-05-11 (redesigned 2026-05-13)

What would it take to make every Rust primitive invisible to user code by default, exposing only what tinct's stdlib explicitly re-exports?

## Design

### The Bootstrap Principle

**No Rust builtin is available to user code by default — not even `+`, `error`, or `=`. The only thing in the bootstrap environment is `include` itself, plus the injected capability caps (`%libdir`, `%pwd`, `%stdin`, `%clock`).**

This is the strongest possible form of primitive privacy: user code can call nothing at all until `prelude.llt` is auto-loaded. Prelude is responsible for making `+`, `error`, `map`, and every other name available. If prelude doesn't export it, users can't call it.

The env chain is:

```text
bootstrap_env = { include, %libdir, %pwd, %stdin, %clock }
  ↓ (prelude auto-loaded here using [include %rust ...])
prelude_output_env = everything prelude.llt defines
  ↓
user env (inherits only prelude_output_env — not bootstrap_env)
```

User code cannot reach `bootstrap_env` or any Rust builtin directly. The only path to a Rust primitive is through a prelude (or stdlib module) that explicitly re-exports it.

### `%rust` — Virtual Module Cap

`%rust` is a special cap value that resolves Rust primitive groups. It is **not** available in user code — it is injected only into the stdlib evaluation context (prelude and files loaded from `%libdir`).

```tinct
# In stdlib/prelude.llt — imports groups of Rust primitives
[include %rust "core"]       # +, -, *, /, =, <, if, error, try, eval, apply, force
[include %rust "string"]     # str, split, replace, trim, upper, lower, str-slice, ...
[include %rust "collection"] # keys, length, merge, append, each, seq, range, ...
[include %rust "json"]       # from-json
[include %rust "meta"]       # type-of, validate, until, llt-repr, tag-of, variant

# In stdlib/io.llt — imports only what io needs
[include %rust "io"]         # open, slurp, write, lines, emit, env, list-dir, ...

# In stdlib/net.llt
[include %rust "net"]        # connect, tls-layer, http2-session, http-request, ...

# In stdlib/math.llt
[include %rust "math"]       # pow, sqrt, sin, cos, log, band, bor, nan?, ...

# In stdlib/datetime.llt
[include %rust "datetime"]   # now, parse-timestamp, timestamp-add, load-tz, ...
```

`%rust` is a `Value::RustRegistry` — an opaque Rust type that cannot be constructed by tinct code. The include resolver recognizes it specially and resolves the module name to a virtual environment containing exactly the named primitive group. No disk access occurs.

### Primitive Groups

| Module | Contents |
|--------|---------|
| `rust::core` | `+`, `-`, `*`, `/`, `%`, `=`, `<`, `if`, `error`, `try`, `eval`, `apply`, `force`, `from-json`-adjacent ops, type predicates (`int?`, `str?`, `dict?`, `fn?`, `seq?`, `null?`, `bool?`, `float?`, `bytes?`, `num?`, `record?`, `map?`), `type-of`, `gensym` |
| `rust::string` | `str`, `split`, `replace`, `trim`, `upper`, `lower`, `starts-with?`, `ends-with?`, `str-chars`, `str-length`, `str-slice`, `str-contains?`, `char-code`, `chr`, `str-bytes`, `bytes-str` |
| `rust::collection` | `keys`, `length`, `merge`, `append`, `get`, `set`, `has?`, `each`, `each-key`, `each-kv`, `seq`, `head`, `tail`, `collect`, `range`, `repeat`, `cycle`, `iterate`, `unfold`, `join`, `concat`, `first`, `last`, `rest`, `cons`, `reverse`, `sort` |
| `rust::bytes` | `bytes`, `bytes-find`, `bytes-of`, `bytes-equal?`, `ct-equal?` |
| `rust::math` | `pow`, `sqrt`, `log`, `log2`, `log10`, `exp`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `nan?`, `inf?`, `finite?`, `floor`, `round`, `to-int`, `to-float`, `float`, `band`, `bor`, `bxor`, `shl`, `shr`, `decimal`, `big-int` |
| `rust::json` | `from-json` |
| `rust::io` | `open`, `slurp`, `write`, `write-atomic`, `write-handle`, `flush`, `close`, `seek`, `seek-end`, `position`, `lines`, `list-dir`, `stat`, `make-dir`, `remove`, `rename`, `link`, `read-link`, `emit`, `env`, `narrow`, `revocable`, `revoke-cap`, `cap-data`, `has-cap?` |
| `rust::net` | `connect`, `tls-layer`, `tls-peer-cert`, `spki-pin`, `send-datagram`, `recv-datagram`, `http-request`, `http2-session`, `http3-session`, `quic-session`, `quic-open-stream`, `quic-open-datagram`, `icmp-ping`, `uri`, `url`, `urn` |
| `rust::datetime` | `parse-timestamp`, `format-timestamp`, `timestamp->unix`, `unix->timestamp`, `now`, `fixed-clock`, `timestamp-add`, `timestamp-diff`, `timestamp<?`, `timestamp>?`, `timestamp=?`, `timestamp-year`, `timestamp-month`, `timestamp-day`, `timestamp-hour`, `timestamp-minute`, `timestamp-second`, `timestamp-parts`, `duration-nanos`, `duration-seconds`, `duration-minutes`, `duration-hours`, `duration-days`, `duration->seconds`, `duration->nanos`, `load-tz`, `timestamp-in-tz`, `local->timestamp`, `local-tz-name` |
| `rust::meta` | `validate`, `until`, `llt-repr`, `tag-of`, `variant`, `eval-ast`, `proxy` |

### What prelude.llt Looks Like

prelude.llt opens by importing the Rust groups it needs, then builds the tinct-level API on top:

```tinct
[include %rust "core"]
[include %rust "string"]
[include %rust "collection"]
[include %rust "json"]
[include %rust "meta"]

# Public tinct API begins here — re-exports and wrappers
[
  # Arithmetic (re-exported from core, wrappers add type coercion / errors)
  +: [fn [a b] [builtin-add a b]]
  ...

  # Higher-level combinators built purely in tinct
  and-then: [fn [result f] [match result [Ok v]: [f v] [Err msg]: [Err msg]]]
  ...
]
```

### What stdlib/io.llt Looks Like

```tinct
[include %rust "io"]

[
  read-file:  [fn [cap path] [try [fn [] [slurp [open cap path "r"]]]]]
  write-file: [fn [cap path content] [write cap path content]]
  read-lines: [fn [cap path] [try [fn [] [lines [open cap path "r"]]]]]
  # ... etc — all built on the raw primitives from rust::io
]
```

### Security Properties

- **User code cannot access `%rust`** — `Value::RustRegistry` is an opaque Rust type; there is no tinct expression that produces one. Even if a user writes `[include %rust "io"]`, `%rust` is undefined in their env.
- **User code cannot spoof `%rust`** — the include resolver checks that the cap is `Value::RustRegistry` at the Rust level, not by name lookup.
- **`include` is the only bootstrap primitive** — not even `+` or `error` exists until prelude loads. A tinct file with no includes evaluates in a universe containing only caps and `include`.
- **stdlib files loaded from `%libdir` receive `%rust` injection** — the include resolver injects `%rust` into the env when evaluating a file loaded from libdir, not from user-controlled paths.

### What Would Change

**`src/value.rs`** — add `Value::RustRegistry` variant (opaque; no payload; PartialEq, Debug, Display).

**`src/builtins.rs`** — remove `create_root_env()` from the user env chain; replace with `create_bootstrap_env()` containing only `include` and the injected caps. Add `fn rust_module(name: &str) -> Rc<RefCell<Environment>>` that dispatches on the module name to return the primitive group env.

**`src/imports.rs`** — `build_prelude_env`: evaluate `prelude.llt` in `bootstrap_env + %rust` (libdir files get `%rust` injected); the resulting env becomes the user env parent. `build_include_env`: when cap is `Value::RustRegistry`, call `rust_module(path)` instead of doing filesystem I/O.

**`stdlib/prelude.llt`** — opens with `[include %rust "core"]`, `[include %rust "string"]`, `[include %rust "collection"]`, `[include %rust "json"]`, `[include %rust "meta"]`; defines the tinct-level API on top.

**`stdlib/io.llt`, `stdlib/net.llt`, `stdlib/math.llt`, `stdlib/datetime.llt`** — each opens with `[include %rust "module-name"]` and builds its tinct-level API on top.

**`builtin-*` aliases** — removed entirely. They were an escape hatch for prelude to call through to Rust primitives when user code shadows the public names. With `%rust` modules, prelude always has direct access to the raw primitives via its imported groups; there is no shadowing concern.

## Prerequisites

- `build_prelude_env` refactor (separating bootstrap from user env)
- `Value::RustRegistry` type added to value.rs

## References

- Racket's `#lang` system — per-file language environments; only the primitives declared by the language are available; no global namespace leakage
- Nix's `builtins` set — a single explicit namespace for all language primitives; stdlib (`nixpkgs`) is a separate layer built on top; users access builtins only through the public API
- Node.js module system — each module has its own scope; nothing leaks between modules unless explicitly exported; `require` is the only bootstrap primitive
