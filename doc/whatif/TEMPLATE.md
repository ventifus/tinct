# What If: [Feature Name] for tinct

**State:** Proposal

What would it take to [add/adopt/implement feature] in tinct?

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

The recommended design. Describe the approach in full: syntax, semantics,
user-facing behavior, internal representation. This is the proposal —
write it as the design, not as one option among many.

Include code examples showing how the feature works:

```tinct
# User-facing syntax
```

```rust
// Internal representation (if relevant)
```

Cover interactions with existing systems (type checker, evaluator,
parser, lazy evaluation, row polymorphism, etc.) as subsections if
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

## Phased Adoption

Break the feature into independently useful phases. Each phase should
be a working system — not a partial implementation that only becomes
useful later.

### Phase 1: [Name]

What it does, what it enables, how to implement it.

### Phase 2: [Name]

...

### Prerequisites

What must be complete before each phase can begin. Reference other
whatif docs and TODO.md sprints by name.

### Trigger

Concrete conditions that should prompt adoption. These are starting
conditions, not reasons to delay. Use specific scenarios:

- "When [specific situation] occurs"
- "When [dependency] is implemented"

## References

Cited papers, language implementations, and specifications. Format:

- Author(s) (Year). "Title." *Venue*, pages. — [relevance to tinct]
- Language documentation. Section. — [what tinct draws from it]
