# Date-Time Support (lib-datetime)

## Overview

tinct provides a complete date-time story — UTC timestamps, duration
arithmetic, timezone conversion from the system database, and a
`ClockCap` capability that makes `now` injectable for unit testing.

`Timestamp` enables certificate expiry checks, scheduled maintenance
window generation, and log retention policies. `Duration` gives
timeouts and intervals typed spans rather than bare integers. `ClockCap`
keeps evaluation hermetic: scripts that don't receive a `ClockCap`
cannot observe the current time, and CI passes
`--cap-clock-fixed "2026-01-01T00:00:00Z"` to make builds reproducible.
Timezone conversion reads from the system zoneinfo database via `DirCap`,
so custom databases and frozen snapshots are used in tests.

## Design

### Value Types

**`Value::Timestamp`** — a UTC instant stored as `i64` nanoseconds since
the Unix epoch (1970-01-01T00:00:00Z). Nanosecond precision; representable
range: approximately 1678–2262 CE (±292 years from epoch).

**Range note:** RFC 5280 permits `99991231235959Z` as a sentinel value
for non-expiring certificates. This overflows `i64` nanoseconds. When
`tls-peer-cert` returns a `not-after` `Timestamp`, certificates using
the RFC 5280 sentinel are clamped to the maximum representable value
(`i64::MAX` nanoseconds, approximately 2262-04-11). Scripts that need
to detect the "does not expire" case check
`[> cert.not-after [parse-timestamp "2200-01-01T00:00:00Z"]]` as a
practical heuristic.

**`Value::Duration`** — a signed span stored as `i64` nanoseconds.
Not calendar-aware (no months or years — calendar arithmetic requires
knowing a reference date). Covers seconds, minutes, hours, and days.

Both are immutable value types with no internal mutability. JSON
serialization: `Timestamp` → RFC 3339 string (e.g.
`"2026-05-07T14:30:00Z"`); `Duration` → ISO 8601 duration string
(e.g. `"PT3600S"` for one hour).

### ClockCap — Injectable Clock Capability

`Value::ClockCap` is a capability for reading the current time.
Scripts receive it from the CLI and pass it to `now`. The capability
has two variants internally:

```rust
enum ClockCapInner {
    Real,           // reads std::time::SystemTime::now()
    Fixed(i64),     // always returns this nanosecond timestamp
}
Value::ClockCap(Rc<ClockCapInner>)
```

**Builtins:**

```text
now clock-cap                → Timestamp   (read current time)
fixed-clock ts               → ClockCap    (always returns ts; for testing)
```

**CLI injection:**

```sh
tinct run --cap-clock clock script.llt
    → binds $clock to the real system clock

tinct run --cap-clock-fixed "2026-01-01T00:00:00Z" clock script.llt
    → binds $clock to a fixed ClockCap returning that timestamp
```

**Unit test pattern:**

```tinct
# In production:
# tinct run --cap-clock clock check-cert.llt

# In tests (deterministic, no real clock):
# tinct run --cap-clock-fixed "2026-05-07T00:00:00Z" clock check-cert.llt

[expiry: [parse-timestamp cert.not-after]]
[days-left: [/ [timestamp-diff expiry [now clock]] [duration-days 1]]]
[if [< days-left 30]
  [error [str "cert expires in " days-left " days"]]
  null]
```

`ClockCap` is consistent with tinct's capability model: `DirCap`
controls filesystem access, `NetCap` controls network access,
`ClockCap` controls time access.

### Timezone via DirCap

The system TZ database lives at `/usr/share/zoneinfo` (Linux, macOS)
and contains IANA timezone files in the compiled `zic` binary format.
tinct reads the system database at runtime through a `DirCap` rather
than shipping a compiled-in timezone database (which would add crate
weight and make the binary stale):

```text
load-tz zoneinfo-dir name    → Timezone
```

`zoneinfo-dir` is a `DirCap` pointing to the zoneinfo directory.
`name` is a string like `"America/New_York"` or `"Europe/Berlin"`.
The function reads and parses the corresponding binary TZ file.

**CLI injection:**

```sh
tinct run --cap-fs zoneinfo=/usr/share/zoneinfo script.llt
```

No new capability type — `DirCap` already provides the right
sandbox semantics. The user controls which zoneinfo directory is used,
which allows custom databases, frozen snapshots for reproducibility,
or mock directories in tests.

**`Value::Timezone`** — an opaque parsed TZ rule set, loaded from a
zoneinfo file. Not serializable. Consumed by timezone conversion
builtins.

```tinct
[include "stdlib/io.llt"]
[include "stdlib/datetime.llt"]

# tinct run --cap-clock clock --cap-fs zoneinfo=/usr/share/zoneinfo script.llt

[tz:    [load-tz zoneinfo "America/New_York"]]
[local: [timestamp-in-tz [now clock] tz]]

# local: [
#   year: 2026  month: 5  day: 7
#   hour: 10  minute: 30  second: 0
#   offset-seconds: -14400
#   name: "EDT"
# ]
```

### Rust Builtins

**Timestamp construction and conversion:**

| Function | Signature | Notes |
|---|---|---|
| `parse-timestamp s` | `String → Timestamp` | RFC 3339 input; errors on invalid format |
| `format-timestamp t` | `Timestamp → String` | RFC 3339 output, always UTC (`Z` suffix) |
| `timestamp->unix t` | `Timestamp → Int` | Unix seconds (truncates nanoseconds) |
| `unix->timestamp n` | `Int → Timestamp` | From Unix seconds |

**Clock:**

| Function | Signature | Notes |
|---|---|---|
| `now cap` | `ClockCap → Timestamp` | Current UTC time |
| `fixed-clock ts` | `Timestamp → ClockCap` | Always returns ts; for testing |

**Timestamp arithmetic:**

| Function | Signature | Notes |
|---|---|---|
| `timestamp-add t d` | `Timestamp → Duration → Timestamp` | Add duration to timestamp |
| `timestamp-diff t1 t2` | `Timestamp → Timestamp → Duration` | `t1 - t2`; positive if t1 is later. Uses `i64::checked_sub`; returns error if difference overflows `i64` nanoseconds (>292 years apart). |
| `timestamp<? t1 t2` | `Timestamp → Timestamp → Bool` | t1 is before t2 |
| `timestamp>? t1 t2` | `Timestamp → Timestamp → Bool` | |
| `timestamp=? t1 t2` | `Timestamp → Timestamp → Bool` | Exact equality |

**Timestamp extraction (UTC):**

| Function | Signature | Notes |
|---|---|---|
| `timestamp-year t` | `Timestamp → Int` | UTC year |
| `timestamp-month t` | `Timestamp → Int` | UTC month (1–12) |
| `timestamp-day t` | `Timestamp → Int` | UTC day of month (1–31) |
| `timestamp-hour t` | `Timestamp → Int` | UTC hour (0–23) |
| `timestamp-minute t` | `Timestamp → Int` | UTC minute (0–59) |
| `timestamp-second t` | `Timestamp → Int` | UTC second (0–59) |
| `timestamp-parts t` | `Timestamp → Dict` | All fields as a dict |

**Duration construction:**

| Function | Signature | Notes |
|---|---|---|
| `duration-nanos n` | `Int → Duration` | n nanoseconds |
| `duration-seconds n` | `Int → Duration` | n × 10⁹ ns |
| `duration-minutes n` | `Int → Duration` | n × 60 × 10⁹ ns |
| `duration-hours n` | `Int → Duration` | n × 3600 × 10⁹ ns |
| `duration-days n` | `Int → Duration` | n × 86400 × 10⁹ ns |
| `duration->seconds d` | `Duration → Int` | Truncates to whole seconds |
| `duration->nanos d` | `Duration → Int` | Exact nanoseconds |

**Timezone:**

| Function | Signature | Notes |
|---|---|---|
| `load-tz zoneinfo-dir name` | `DirCap → String → Timezone` | Parse IANA TZif binary file. Returns error (never panics) on any parse failure, including malformed files. |
| `timestamp-in-tz t tz` | `Timestamp → Timezone → Dict` | UTC→local conversion; returns year/month/day/hour/minute/second/offset-seconds/name |
| `local->timestamp y mo d h mi s tz` | `Int×6 → Timezone → Timestamp` | Local→UTC |
| `local-tz-name zoneinfo-dir` | `DirCap → String` | System local TZ name (e.g. `"America/New_York"`) |

**`stdlib/datetime.llt` pure-tinct additions:**

```tinct
# stdlib/datetime.llt

# Days between two timestamps (positive if t1 is later)
days-between: [fn@Integer [t1@Timestamp t2@Timestamp]
  [/ [duration->seconds [timestamp-diff t1 t2]] 86400]]

# Is a timestamp within a half-open interval [start, end)?
timestamp-in-range?: [fn@Boolean [start@Timestamp end@Timestamp t@Timestamp]
  [and [not [timestamp<? t start]] [timestamp<? t end]]]

# Format a timestamp as a date-only string "YYYY-MM-DD" (UTC)
format-date: [fn@String [t@Timestamp]
  [str [timestamp-year t] "-"
       [pad-left 2 [str [timestamp-month t]]] "-"
       [pad-left 2 [str [timestamp-day t]]]]]
```

### Scope Limits

**No calendar arithmetic.** Adding months or years is not supported —
"one month from January 31" is ambiguous. Scripts that need this
use `duration-days` with explicit day counts.

**No locale-aware formatting.** `format-timestamp` always produces
RFC 3339. Custom format strings (strftime-style) are out of scope.

**No DST gap/fold handling at the tinct level.** `local->timestamp`
uses the `chrono`/`jiff` crate's disambiguation strategy (prefers the
first occurrence in gaps/folds). Scripts that must handle DST
transitions precisely use UTC throughout and convert only for display.

**UTC extraction only.** `timestamp-year` etc. return UTC fields.
For local fields, use `timestamp-in-tz` first.

## Implementation

### New Value Variants (`src/value.rs`)

- `Value::Timestamp(i64)` — nanoseconds since Unix epoch
- `Value::Duration(i64)` — signed nanoseconds
- `Value::ClockCap(Rc<ClockCapInner>)` — real or fixed clock
- `Value::Timezone(Rc<TimezoneData>)` — parsed IANA TZ rules

### Rust Builtins (`src/builtins.rs`)

~25 new builtins across timestamp construction, arithmetic, extraction,
duration, clock, and timezone. Registered in `datetime_builtins()` in `src/builtins_datetime.rs`, accessible via `builtin_module("datetime")`.

### CLI (`src/main.rs`)

- `--cap-clock NAME` — inject real ClockCap as `$NAME`
- `--cap-clock-fixed "RFC3339" NAME` — inject fixed ClockCap as `$NAME`

### Type Checker (`src/typecheck.rs`)

```text
Timestamp, Duration, ClockCap, Timezone — four new Type variants
now          : ClockCap → Timestamp
fixed-clock  : Timestamp → ClockCap
parse-timestamp : String → Timestamp
format-timestamp : Timestamp → String
timestamp-add  : Timestamp → Duration → Timestamp
timestamp-diff : Timestamp → Timestamp → Duration
load-tz        : DirCap → String → Timezone
timestamp-in-tz : Timestamp → Timezone → Dict
duration-days  : Int → Duration
-- etc.
```

### TOML Integration

`stdlib/toml-lite.llt` parses TOML Offset Date-Time values into
`Timestamp` directly via `parse-timestamp`.

### JSON Serialization

- `Timestamp` → RFC 3339 string (`"2026-05-07T14:30:00.000000000Z"`)
- `Duration` → ISO 8601 duration string (`"PT3600S"`)
- `ClockCap`, `Timezone` → not serializable (capabilities/opaque)

## Dependencies

- `jiff = "0.1"` — the implementation crate. jiff bundles `jiff-tzdb`
  (a compiled-in copy of the IANA database) by default. To read the
  system `/usr/share/zoneinfo` via `DirCap`, the implementation uses
  `TimeZoneDatabase::from_dir(path)` not the default
  `TimeZoneDatabase::bundled()`. The `jiff-tzdb` bundled default must
  be disabled (`default-features = false`) to avoid shipping two TZ
  databases.
- `--cap-clock-fixed "RFC3339" NAME` CLI flag: the RFC 3339 string
  is parsed and converted to `i64` nanoseconds during CLI argument
  processing. Dates outside the ±292-year range overflow `i64`; the CLI
  validates the parsed date fits in range and returns a user-visible
  error, not a panic or silent wrap.
- `pad-left` from `doc/feature/lib-supplemental.md` §Extended String
  Utilities (used in `format-date`).
- `DirCap` and file reading from `doc/whatif/io.md` (accepted).
- `ClockCap` CLI flags extend `src/main.rs` alongside existing
  `--cap-fs` / `--cap-net` flags.

## References

- RFC 3339. Klyne, G. & Newman, C. (2002). "Date and Time on the
  Internet: Timestamps." IETF. — The wire format for Timestamp
  serialization and `parse-timestamp` / `format-timestamp`.
- ISO 8601. International Organization for Standardization (2019).
  "Date and time — Representations for information interchange." —
  Duration string format (`PT3600S`).
- Olson, A. (1986–). IANA Time Zone Database. iana.org/time-zones. —
  The zoneinfo files read by `load-tz` from the system directory.
- Flate, P. et al. zic(8) man page. — Binary format of compiled
  IANA TZ files that `load-tz` parses from the DirCap directory.
- Gallant, A. (2024). "jiff" crate. github.com/BurntSushi/jiff. —
  Modern Rust date-time library with first-class system TZ database
  support; the implementation crate.
- Miller, M.S. (2006). *Robust Composition*. PhD thesis, Johns
  Hopkins University. — `ClockCap` as a time-access capability:
  scripts that don't receive `ClockCap` cannot observe the current
  time, enabling hermetic/reproducible evaluation.
