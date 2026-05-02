# Introduction

## Vision

**One language for data AND logic.** Tinct is a unified data representation and transformation language. It combines the simplicity of JSON with the power of functional transformation languages like JSONnet and jq, with lazy evaluation throughout.

```
Traditional:  JSON (data) + jq (transformation) = Two languages
Tinct:          Tinct (data + transformation)       = One language
```

### Dual Purpose

**Data representation** — Humans and LLMs define complex data structures following composition and DRY principles. The syntax is compact and readable, with less punctuation than JSON.

**Data transformation** — The same language expresses lazy-evaluating functional transformations. There is no separate syntax for "data" vs "queries" vs "templates."

### Pipeline Model

Data flows through stages. Within a file, `---` separates independent documents. Each document's output becomes `%` for the next:

```
file.llt
├── document 1 (data)         → % for doc 2
├── ---
├── document 2 (transform)    → % for doc 3
├── ---
└── document 3 (output)       → final value, serialized by CLI
```

Within a document, sequential expressions form a scope chain — each expression's bindings are visible to the next.

### LLM-Friendly

Designed for LLMs to generate and modify:
- Fewer tokens than JSON (no mandatory quotes on keys, no commas)
- Consistent syntax — everything is `[key: value]` or `[f args]`
- Composition eliminates repetition, reducing token count further

---

## Core Principles

### Principle 1: Dicts Are Fundamental

The lowest-level unit is the dictionary (key-value pairs), not the list. First-class key-value pair syntax is core to the language.

A list is equivalent to a dict with integer keys:

```tinct
[a b c]  ≡  [0: a  1: b  2: c]
```

**Why this design:**
- **Unification** — One fundamental data structure. Functions like `map`, `filter`, `get` work uniformly on all data.
- **Flexibility** — Mixed integer and string keys naturally supported. Natural extension to keyword arguments.
- **First-class key-value pairs** — Matches the configuration language use case. Keys are names, not duplicated strings.

**Implementation:** May use different internal representations (dense vector for list-like data, HashMap for sparse/string keys) as a transparent performance optimization. Users never see the difference.

**Type-theoretic implication:** The static `Record` type tracks only string-keyed fields; integer-keyed (positional) entries are not part of the record type. A dict `[a b c  name: Alice]` has record type `[name: String]` — the positional entries `a`, `b`, `c` are invisible to the type checker. This is a deliberate consequence of unifying lists and records: positional entries are list-like data without static field names, while named entries form the record structure that type inference reasons about.

### Principle 2: One Bracket, One Structure

**`[]` is the only bracket type.** There is one syntax for the one fundamental data structure. Entries with `key:` are keyed; entries without get auto-incrementing integer keys. Both can appear in the same `[]`.

```tinct
[name: "Alice"  age: 30]        # All keyed — a "dict"
[a b c]                         # All auto-indexed — a "list" = [0: a  1: b  2: c]
[f x timeout: 60]               # Mixed — positional + named (implied call)
[]                              # Empty — list and dict are identical
```

**Parsing rule:** After parsing an entry, look ahead for `:`. If found, the entry is a key and the next thing is its value. If not, the entry is auto-indexed. The integer counter only increments for unkeyed entries — keyed entries don't consume an index.

**Positional and named entries may appear in any order.** Auto-indices are assigned sequentially to positional entries regardless of where named entries appear. For function calls, the binding priority chain (§Call Convention, C-PRIORITY) resolves positional arguments by index, then named arguments fill remaining parameters, then defaults apply.

### Principle 3: Implied Call — Bare Identifier in Head Position

**A bare identifier in head position signals function application.** Brackets containing an identifier head are calls. `$` on the head element prevents call interpretation, making the bracket a data sequence.

```tinct
[a b c]                # Call: a(b, c) — bare identifier in head
[f x y]                # Call: f(x, y)
[$f x y]               # Data: sequence [ref(f), ref(x), ref(y)] — $ prevents call
```

Syntactically, `[f x]` is a bracket expression with unkeyed entries (the same parsing mechanism as `[a b c]`). The bare identifier `f` in head position triggers call interpretation: the parser interprets the head as the function and remaining entries as arguments. The AST represents this as a `Call` node with `func`, `args`, and `named_args` — not as a dict.

**Why:** Enables full lazy evaluation. The head-position rule is a parser-level decision (made before evaluation), so the evaluator never needs to eagerly inspect the head of a bracket expression to determine its role. The entire application (including the function) can remain a thunk until materialized.

**Parser recognition:** The parser checks the first entry of every `[]`. If it matches a keyword (`call`, `fn`, `type`), the parser emits a specialized AST node. If it's a bare identifier (not a keyword, not followed by `:`), it's an implied call. Otherwise it emits a `Dict` node. This is a parser-level decision, not an evaluator-level one.

```tinct
[f x y]                        # Parsed as CallExpr (implied call) — requires exact arity
[call f x]                     # Parsed as CallExpr (explicit call) — same AST as [f x]
[fn [x] [+ x 1]]               # Parsed as FnExpr — function definition
```

**Edge cases:**
- `[call: something]` — the `:` makes `call` a key, not a keyword. Parsed as `Dict`.
- `[f]` — zero-argument call to `f`. To construct a single-element data sequence containing a reference, use `[$f]`.

**`call` remains valid.** Both `[f x]` and `[call f x]` produce identical AST. The `call` keyword is required when the function is a computed expression rather than a bare identifier (e.g., `[call [get-handler request] data]`).

### Principle 4: Lazy Evaluation

Everything is a thunk until materialized. Compute only what's needed, when it's needed.

```tinct
[
    # Won't run unless `result` is actually used
    result: [expensive-computation data]

    # Infinite sequences -- only compute what you take
    naturals: [range 0]
    first-ten-evens: [collect
        [take 10
            [filter [fn [n] [= 0 [mod n 2]]] naturals]]]

    # Short-circuit: if condition is true, never evaluate the else branch
    value: [if condition cheap-option very-expensive-option]
]
```

### Principle 5: Composition Over Duplication

Build complex things from simple things. No repetition.

```tinct
[
    base: [timeout: 30  retries: 3]
    dev:  [merge base [env: "dev"]]
    prod: [merge base [env: "prod"  timeout: 60]]
]
```

Compare to JSON where every field must be repeated:
```json
{
  "dev":  {"timeout": 30, "retries": 3, "env": "dev"},
  "prod": {"timeout": 60, "retries": 3, "env": "prod"}
}
```
