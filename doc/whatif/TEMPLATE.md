# What If: [Feature Name] for tinct

**State:** Proposal
<!-- Optional — include when this whatif supersedes an earlier one: -->
<!-- **Replaces:** [`earlier-name.md`](earlier-name.md) — one sentence on what specifically this supersedes -->
<!-- When superseded by another whatif: **Superseded by:** [`successor.md`](successor.md) -->
<!-- When superseded by a sprint decision (no successor whatif): **Resolved by:** `sprint-slug` (DONE.md) -->

What would it take to [add/adopt/implement feature] in tinct?

## Goals

1. **[Primary goal].** One sentence on what this achieves and why.
2. **[Secondary goal].** ...
3. **[Tertiary goal].** ...

## Current State

How tinct handles this area today. What exists, what works, what's
missing. Reference doc/*.md chapters, source files, and existing
builtins/stdlib functions.

Include code examples showing the current behavior or workaround:

```tinct
# Current approach (verbose, limited, etc.)
```

### What's Missing

Numbered list of concrete gaps the feature would fill.

## Why [Feature] Matters for tinct

The concrete value — what becomes possible that isn't today. Focus on
user-facing benefits, not implementation details. Each item should be
a capability, not a mechanism.

## Design

The complete, fully realized design. Describe the feature as it will
exist when done: syntax, semantics, user-facing behavior, internal
representation. Write as the definitive end state — no phases, no
"initially we could", no hedging.

Include code examples:

```tinct
# User-facing syntax
```

```rust
// Internal representation (if relevant)
```

Cover interactions with existing systems (type checker, evaluator,
parser, lazy evaluation, row polymorphism, etc.) as subsections where
they involve non-obvious design choices.

## What Would Change

Concrete impact on tinct subsystems. One subsection per affected area.
For each: current state, proposed change, impact assessment (Minor /
Moderate / Major / Fundamental).

### [Subsystem 1]

**Current:** ...
**Proposed:** ...
**Impact:** ...

### [Subsystem 2]

...

## Prerequisites

What must be complete before this feature can be implemented. Reference
other whatif docs and TODO.md sprints by name. Use concrete dependencies,
not vague "when needed."

## References

Cited papers, language implementations, and specifications. Format:

- Author(s) (Year). "Title." *Venue*, pages. — [relevance to tinct]
- Language documentation. Section. — [what tinct draws from it]
