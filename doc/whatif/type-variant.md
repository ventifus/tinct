# What If: Type::Variant for Transport and Protocol Constants

**State:** Superseded — the correct approach is to declare `[union Transport [Tcp] [Udp] [Quic] [Unix]]` using the existing nominal variants machinery, not to add a new `Type::Variant`. See `transport-typing` sprint in TODO.md.
**Superseded by:** [`completed/nominal-variants.md`](completed/nominal-variants.md)

What would it take to add a `Type::Variant` to tinct's type system to precisely type Transport protocol constants and other nominal variant values?

## Current State

Transport protocol constants (`Tcp`, `Udp`, `Quic`, `Unix`) are registered in `src/type_env.rs` as `Type::Unknown`:

```rust
// Current — no type info
env.insert("Tcp", TypeScheme::mono(Type::Unknown));
env.insert("Udp", TypeScheme::mono(Type::Unknown));
```

The `connect` builtin accepts a transport parameter typed as `Unknown`. This means:
- No compile-time checking that `Tcp` is passed to `connect` instead of `42`
- No exhaustiveness checking in `match` arms over transport values
- No LSP hover showing what variants are available

### What's Missing

1. A `Type::Variant(String)` — a nominal opaque variant type that carries a tag name
2. Transport constants typed as `Type::Variant("Transport")` (or similar tagged union)
3. `connect` typed to accept `Type::Variant("Transport")` not `Unknown`
4. Pattern matching exhaustiveness for variant types

## Why Type::Variant Matters for tinct

**Protocol safety**: `[connect cap Tcp "localhost" 8080]` vs `[connect cap "tcp" "localhost" 8080]` — without Variant types, passing a bare string "tcp" instead of the `Tcp` constant is a silent runtime failure.

**API discoverability**: LSP can surface the available transport variants in completion and hover.

**Exhaustiveness**: `[match transport [Tcp: ...] [Udp: ...]]` can warn when `Quic` and `Unix` are unhandled.

## Design

```rust
// Type::Variant("Transport") — nominal opaque variant
// A value whose tag is a known constructor name from a declared union

// In type_def.rs
Type::Variant(String),  // e.g. Type::Variant("Transport")
```

Transport constants registered as:
```rust
env.insert("Tcp", TypeScheme::mono(Type::Variant("Transport".to_string())));
env.insert("Udp", TypeScheme::mono(Type::Variant("Transport".to_string())));
env.insert("Quic", TypeScheme::mono(Type::Variant("Transport".to_string())));
env.insert("Unix", TypeScheme::mono(Type::Variant("Transport".to_string())));
```

`connect` typed as:
```rust
Type::Function {
    params: vec![
        (Some("cap".to_string()), Type::NetCap),
        (Some("transport".to_string()), Type::Variant("Transport".to_string())),
        ...
    ],
    ...
}
```

Subtyping: `Type::Variant("X") <: Type::Variant("X")` (reflexive). Two different variant tags are disjoint.

## What Would Change

### src/type_def.rs

**Current:** No `Type::Variant` variant.
**Proposed:** Add `Variant(String)` variant; update all exhaustive matches (50+ sites).
**Impact:** Major — touches every Type match site.

### src/type_env.rs

**Current:** Transport constants registered as `Type::Unknown`.
**Proposed:** Registered as `Type::Variant("Transport")`.
**Impact:** Minor — 4 registration changes.

### src/type_unify.rs

**Current:** No Variant-specific unification rules.
**Proposed:** `Variant(tag1) ~ Variant(tag2)` iff `tag1 == tag2`.
**Impact:** Minor — one arm in unify.

## Prerequisites

None — `Type::Variant` is independent of HKT, CHR, or other in-progress features.

## References

- Jones, M.P. (1999). "Typing Haskell in Haskell." *Haskell Workshop.* — nominal type tagging
- Pierce, B. (2002). *Types and Programming Languages.* MIT Press. §11.4 Variants.
