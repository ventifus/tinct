# What If: Pattern Matching

## Status

Stub — not yet researched.

## Problem

tinct has no pattern matching construct. Type-based dispatch requires
verbose `$type-of` + string comparison chains:

```lisp
[call $if [call $= [call $type-of $x] Dict]
    [call $handle-dict $x]
    [call $if [call $= [call $type-of $x] Seq]
        [call $handle-seq $x]
        [call $error "unexpected type"]]]
```

This matters because:

1. **Self-hosting builtins** — dual-dispatch builtins (`$map`, `$filter`,
   `$reduce`, `$join`, `$take`, `$drop`) are in Rust primarily because they
   need to pattern-match on `Value::Dict` vs `Value::Seq`. With pattern
   matching + more granular primitives, tinct could define the dispatch
   wrapper and keep only the type-specific implementations in Rust.

2. **User code** — any polymorphic data processing (handling different
   shapes, optional fields, tagged unions) is awkward without matching.

3. **Error handling** — `$try` returns `[ok: value]` or `[err: message]`,
   and dispatching on the result key is clunky without destructuring.

## Current State

- `$type-of` returns a string (`"Int"`, `"Float"`, `"Dict"`, `"Seq"`, etc.)
- `$seq?` is the only type predicate builtin (returns Bool)
- `$if` + `$=` chains are the only dispatch mechanism
- No destructuring bind
- No exhaustiveness checking
- Listed in DESIGN.md §Future Features as "Not yet designed"

## Questions to Research

- What form should match syntax take in tinct's bracket-based grammar?
- Should matching be a special form (`[match ...]`) or a builtin function?
- Destructuring: structural (dict keys, seq head/tail) or just type dispatch?
- Exhaustiveness checking: static (type system) or runtime (default branch)?
- Interaction with lazy evaluation: when are matched values forced?
- Interaction with row polymorphism: matching on partial record shapes?
- How do comparable languages handle this? (Nix `builtins.typeOf`, Jsonnet
  `std.type`, Elixir `case`, Haskell `case`, Nickel `match`)

## References

- DESIGN.md §Future Features: "Pattern matching — Not yet designed."
- doc/whatif/macros.md §Recommendation: pattern matching identified as the
  biggest enabler for self-hosting more Rust builtins in tinct.
