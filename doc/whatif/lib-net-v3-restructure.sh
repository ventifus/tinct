#!/usr/bin/env bash
set -euo pipefail

SRC="/var/home/adenton/Projects/tinct/doc/whatif/lib-net-v3.md"
TMP="/var/home/adenton/Projects/tinct/doc/whatif/lib-net-v3.md.tmp"

# Line number reference (1-indexed, inclusive):
#
# 1-15    Header (front-matter + Resolved/Extends lines)
# 17      ## Problem  (heading)
# 19-21   Problem body
# 23-39   ## Design Principle: Subject Last
# 42-70   ## The Proposal  (DROPPED)
# 74-127  ## The Symmetry
# 130-157 ## Error Philosophy
# 159-215 ## The `Transport` and `Protocol` Typeclasses
# 217-258 ## Transparent Handle Design
# 260-529 ## Connection Interfaces: `ByteStream` and `Datagram`
#           (incl. ### ByteStream 264-338, ### Datagram 339-369,
#            ### MessageStream 370-529 incl. ### %emit 421-529)
# 530-625 ### `Codec`  (subsection inside Connection Interfaces)
# 626-634 ### Summary: the four IO shapes
# 635-650 ### What Changes in the Implementation
# 652-690 ## Server Layers  heading + codec-stream/codec-sink/stream-drain subsections
# 691-783 ### `channel-map` and `channel-flat-map`
# 785-827 ## Client Layers
# 829-851 ## Bidirectional Connections  (851 = last content line before ---)
# 854-878 ## Transport-Agnostic Application Protocols  (body before DNS subsection)
# 879-920 ### DNS as the Worked Example
# 923-976 ## Worked Example: ICMP Ping Tunnel over H3
# 979-1020 ## Worked Example: Simple HTTP Server
# 1022-1163 ## Worked Example: HTTP Client with SVCB/HTTPS Records
# 1165-1238 ## New Rust Primitives
# 1240-1283 ## Stdlib Module Map
# 1285-1415 ## Fixed-Size Bytes: `[Bytes N]`
# 1417-1728 ## What Would Change
# 1730-1733 ## Prerequisites
# 1736-1753 ## References

{

# ── 1. Header (front-matter) ───────────────────────────────────────────────────
sed -n '1,15p' "$SRC"

# ── 2. Problem ────────────────────────────────────────────────────────────────
printf '\n---\n\n'
sed -n '17,21p' "$SRC"

# ── 3. Architecture: Seven Layers (NEW) ───────────────────────────────────────
cat <<'ARCH'

---

## Architecture: Seven Layers

| Layer | Name | Contents |
|---|---|---|
| 1 | Rust Primitives | tcp-bind/connect, udp-socket/recv/send, read/write, crypto primitives |
| 2 | IO Typeclasses | ByteStream, Datagram, Seekable, MessageStream + Channel instance |
| 3 | Codecs | Codec typeclass — data transformations enabling encryption, framing, compression |
| 4 | Protocol Layers | tls, quic, h2, h3, ws, wireguard, noise — composable with \| |
| 5 | Serve/Connect Patterns | Transport/Protocol typeclasses, serve, drain, select |
| 6 | Full Stack Compositions | Worked examples |
| 7 | Convenience Functions | https-channel, http-channel |
ARCH

# ── 4. Design Principles ───────────────────────────────────────────────────────
cat <<'DP'

---

## Design Principles

DP

# §Design Principle: Subject Last — heading + body (lines 23-39)
sed -n '23,39p' "$SRC"

printf '\n---\n\n'

# §Error Philosophy — heading + body (lines 130-157)
sed -n '130,157p' "$SRC"

printf '\n---\n\n'

# §Transparent Handle Design — heading + body (lines 217-258)
sed -n '217,258p' "$SRC"

# ── 5. Layer 1 — Rust Primitives ──────────────────────────────────────────────
cat <<'L1'

---

## Layer 1 — Rust Primitives

Everything above Layer 1 is tinct.

L1

# §New Rust Primitives — heading + body (lines 1165-1238)
sed -n '1165,1238p' "$SRC"

# ── 6. Layer 2 — IO Typeclasses ───────────────────────────────────────────────
cat <<'L2'

---

## Layer 2 — IO Typeclasses

L2

# §Connection Interfaces: `ByteStream` and `Datagram` — heading (line 260)
# + ByteStream (264-338) + Datagram (339-369) + MessageStream (370-529 incl. %emit)
# Stop before ### `Codec` at line 530
sed -n '260,529p' "$SRC"

printf '\n'

# §Summary: the four IO shapes (lines 626-634)
sed -n '626,634p' "$SRC"

# ── 7. Layer 3 — Codecs ───────────────────────────────────────────────────────
cat <<'L3'

---

## Layer 3 — Codecs

L3

# §`Codec` subsection (lines 530-625)
sed -n '530,625p' "$SRC"

# ── 8. Layer 4 — Protocol Layers ──────────────────────────────────────────────
cat <<'L4'

---

## Layer 4 — Protocol Layers

L4

# §The Symmetry — heading + body (lines 74-127)
sed -n '74,127p' "$SRC"

printf '\n---\n\n'

# §Client Layers — heading + body (lines 785-827)
sed -n '785,827p' "$SRC"

printf '\n---\n\n'

# §channel-map and channel-flat-map from §Server Layers (lines 691-783)
sed -n '691,783p' "$SRC"

printf '\n---\n\n'

# §Bidirectional Connections — heading + body (lines 829-851)
sed -n '829,851p' "$SRC"

# ── 9. Layer 5 — Serve/Connect Patterns ───────────────────────────────────────
cat <<'L5'

---

## Layer 5 — Serve/Connect Patterns

L5

# §The Transport and Protocol Typeclasses — heading + body (lines 159-215)
sed -n '159,215p' "$SRC"

printf '\n---\n\n'

# §Server Layers heading + codec-stream/codec-sink/drain-emit + stream-drain/serve-streams
# (lines 652-690, stopping before channel-map at 691)
sed -n '652,690p' "$SRC"

printf '\n---\n\n'

# §Transport-Agnostic Application Protocols — heading + body (lines 854-878)
# (stops before ### DNS as the Worked Example at 879)
sed -n '854,878p' "$SRC"

# ── 10. Layer 6 — Full Stack Compositions ────────────────────────────────────
cat <<'L6'

---

## Layer 6 — Full Stack Compositions

L6

# §DNS as the Worked Example (lines 879-920)
sed -n '879,920p' "$SRC"

printf '\n---\n\n'

# §Worked Example: ICMP Ping Tunnel over H3 (lines 923-976)
sed -n '923,976p' "$SRC"

printf '\n---\n\n'

# §Worked Example: Simple HTTP Server (lines 979-1020)
sed -n '979,1020p' "$SRC"

printf '\n---\n\n'

# §Worked Example: HTTP Client with SVCB/HTTPS Records (lines 1022-1163)
sed -n '1022,1163p' "$SRC"

# ── 11. Layer 7 — Convenience Functions (NEW) ────────────────────────────────
cat <<'L7'

---

## Layer 7 — Convenience Functions

`https-channel` and `http-channel` are pre-composed stacks from Layers 1-4. The explicit pipeline form is always available and shows the full stack.

---

L7

# ── 12. Stdlib Module Map ─────────────────────────────────────────────────────
sed -n '1240,1283p' "$SRC"

printf '\n---\n\n'

# ── 13. Fixed-Size Bytes: `[Bytes N]` ─────────────────────────────────────────
sed -n '1285,1415p' "$SRC"

# ── 14. Implementation Details ────────────────────────────────────────────────
cat <<'IMPL'

---

## Implementation Details

IMPL

# §What Would Change subsections (lines 1418-1728, skip heading line 1417)
sed -n '1418,1728p' "$SRC"

# §What Changes in the Implementation subsection from Connection Interfaces (lines 635-650)
sed -n '635,650p' "$SRC"

printf '\n---\n\n'

# ── 15. Prerequisites ─────────────────────────────────────────────────────────
sed -n '1730,1733p' "$SRC"

printf '\n---\n\n'

# ── 16. References ────────────────────────────────────────────────────────────
sed -n '1736,1753p' "$SRC"

} > "$TMP"

mv "$TMP" "$DST"
echo "Done: $DST"
wc -l "$DST"
