# What If: Unified Access and Generator Pipeline for tinct

**State:** Accepted — 2026-05-05

What would it take to replace tinct's bracket-access syntax with a unified `|` pipeline operator and generator-native data transformation?

## Current State

tinct has three access forms, all backed by separate AST nodes:

```tinct
config.host               # dot: static string key (DotAccess)
data["key"]               # bracket: any key expression (BracketAccess)
data[$key]                # bracket: dynamic key via variable
data[2..5]                # range: key-range slice (RangeAccess)
```

Dot access is clean and familiar. Bracket access is the workhorse for dynamic and integer keys. Range access is a compact slice notation. Together they form a complete access language — but one with asymmetries:

- `.` is the only true infix operator in the language
- `..` is special syntax only valid *inside* bracket-access frames — a mini-language-within-a-mini-language
- The whitespace-sensitive `[` detection (`$a[0]` vs `$a [0]`) is the most complex part of the lexer, required solely to support bracket access
- Dynamic key access (`$a[$key]`) breaks the natural dot-chain reading: `config.host` and `config[$key]` look different even when they do the same thing

For multi-step data transformation, tinct uses `->` (the threading function in the stdlib):

```tinct
[-> users
  [map [fn [u] u.name]]
  [filter [fn [n] [not [= n ""]]]]]
```

This works, but `->` is a prefix function call — the data subject appears first, then gets buried inside `[-> ...]`. It doesn't compose visually with access chains.

For sequence operations, `Value::Seq` (lazy cons-list) already exists as the carrier for `range`, `repeat`, `cycle`, `filter` results, and similar operations. `collect` materializes a Seq back to a Dict. The type system tracks `Type::Seq`. This infrastructure is complete.

### What's Missing

1. **Computed dot-style access.** `$data[$key]` works but breaks the chain reading style. There's no `$data.$key` form.
2. **Integer dot access.** `$data.0` doesn't work — `DotAccess` only does string-key lookup. Integer-keyed lists require `$data[0]`.
3. **A unified infix operator** that composes access, dynamic lookup, and transformation into one coherent left-to-right reading model.
4. **Generator-native transformation.** Going from `$data | [each] | [fn [u] u.name]` to a Seq of names requires either `map` (prefix, data-last) or `->` (prefix threading). There's no way to write a readable left-to-right pipeline that both accesses and transforms.
5. **Projection and path drilling.** Selecting multiple keys (`$data | [select "name" "age"]`) or drilling a computed path (`$data | [path "users" 0 "name"]`) have no syntax — they'd require nested stdlib calls.

## Why This Matters for tinct

tinct's mission has expanded: it is a **general-purpose programming language that puts structured data first**. The config-language era's access model (dot for static fields, brackets for everything else) is no longer sufficient. Data transformation programs need:

1. **Left-to-right pipelines.** `$users | [each] | [where active?] | [fn [u] u.name] | [collect]` reads in execution order. The prefix `[map [fn [u] u.name] [filter active? users]]` reads inside-out.

2. **Generator-native iteration.** jq's central insight: when a pipe stage produces multiple values, downstream stages receive each one. `$users | [each] | .name` in jq extracts all names without a `map`. tinct can have this — `Value::Seq` already exists as the right carrier.

3. **Uniform field access.** Static (`$data.name`) and dynamic (`$data | $key`) should feel like the same operation with different key-expression syntax, not two completely different syntactic forms.

4. **Grammar simplification.** Removing bracket access eliminates the whitespace-sensitive `[` lexer complexity — one of the hardest parts of the current parser to understand and maintain.

## Design

The redesign has three components: extend dot access, add the `|` operator, and add generator primitives.

### Component 1: Unified Dot Access

Dot access becomes the *only* access syntax, extended to handle all key types:

```tinct
$data.name          # string key (existing)
$data.0             # integer key (new — currently fails for Key::Int)
$data.0.name        # chain: integer then string
```

When the token after `.` is an integer literal, the evaluator looks up `Key::Int(n)` instead of `Key::String("0")`. This matches Nix's behavior where `list.0` accesses the first element.

Bracket access (`$a["key"]`, `$a[0]`, `$a[$key]`) and range access (`$a[0..5]`) are **removed**. The tokens `BracketAccess` and `Range` (`..`) are removed from the lexer along with the whitespace-sensitive `[` detection. The AST nodes `BracketAccess` and `RangeAccess` are removed. The `BracketAccessKey` parser frame is removed.

### Component 2: The `|` Pipe Operator

`|` is tinct's second infix operator alongside `.`. It is eliminated entirely in the desugar pass — `lhs | rhs` is rewritten to a function call before evaluation. No runtime dispatch occurs.

**Desugar rules:**

```
lhs | [f args...]  →  [f args... lhs]   # append lhs as final positional argument
lhs | name         →  [name lhs]         # bare word: apply name as function to lhs
lhs | other        →  [other lhs]        # any other expression: call it with lhs
```

`|` is left-associative, so chains desugar left-to-right:

```tinct
users | each | [map fn] | collect
→ [collect [map fn [each users]]]
```

**The N-1 rule.** Arity checking enforces the constraint automatically. `[map fn]` standalone is an arity error — `map` needs two arguments. `users | [map fn]` desugars to `[map fn users]` — a valid two-argument call. Only the RHS of `|` gets the extra argument added.

`[f: [map fn]]` — storing `[map fn]` as a dict entry value — is an arity error because it is evaluated standalone, not in pipe context.

**Partial application is deferred.** First-class partial values are a separate feature. For now, `[map fn]` is only valid as the RHS of `|`.

**Dynamic field access: `get`.** `|` does not dispatch on String or Int RHS values — those are type errors. Dynamic key lookup uses the `get` stdlib function (curried, data-last):

```tinct
dict | [get "name"]      # static string key — point-free
dict | [get $key]        # dynamic computed key
dict | [get 0]           # integer index
dict | [fn [u] u.name]   # or explicit lambda — programmer's choice
```

`get : Key → Record → Any` returns a function `Record → Any` when given one argument, composing naturally with `|`.

**Call-head position and `$`.** A bare word is in *call-head position* only as the first token of a `[...]` bracket form. Everywhere else — including the RHS of `|` — bare words are variable references, and `$` is unnecessary:

| Context | Bare word | `$`-prefixed |
|---------|-----------|--------------|
| First token of `[...]` | call-head (function to call) | VarRef (forced reference) |
| Anywhere else | VarRef | VarRef (redundant `$`) |

```tinct
users | each | [map fn] | collect   # bare words after | are VarRefs — no $ needed
[f [$x | g]]                         # $ needed: x in call-head position of inner [...]
```

**Precedence:** `.` > call > `|`. `|` terminates call argument accumulation inside `[...]`:

```tinct
[f x | g]       # = ([f x]) | g  ← common case: pipe binds looser than call args
[f [$x | g]]    # = call f with (x piped through g) — explicit grouping
a.b | c.d       # dot chains are atomic RHS: Pipe(DotAccess(a,b), DotAccess(c,d))
```

**Chaining is left-associative:**

```tinct
users | each | [filter active?] | [map _.name] | collect
# desugars to: [collect [map _.name [filter active? [each users]]]]
```

**`|` inside dict entries** parses correctly. The `:` lookahead for entry boundaries is unaffected:

```tinct
[key: a | f   other: x]     # key gets (a | f), other gets x
```

**`$_` desugaring.** `$_` on the LHS of `|` (or a DIRECT chain from `$_` as LHS) triggers lambda wrapping — the whole pipe becomes `[fn [_] $_ | f]`. `is_direct_underscore` is extended to cover `Pipe { lhs, .. }` by recursing into lhs. `$_` in RHS call arguments wraps that specific argument as in any other call; it does not wrap the whole pipe. Useful RHS patterns:

- Bare word: `... | each`, `... | collect`
- Field shorthand: `... | _.name` (desugars to `[fn [__0] __0.name]`)
- Explicit lambda: `... | [fn [u] u.name]`
- Partial call: `... | [map _.name]`, `... | [get "name"]`

**No implicit Seq distribution.** `|` applies its RHS to the LHS value as a whole — there is no automatic element-wise distribution over Seqs. `users | [length]` gives the length of the Seq. Use `each` to iterate explicitly.

**Relation to `->`.** `->` (stdlib threading) remains for cases where the stage list is a runtime value. The two differ for Seq values: `->` applies each stage to the whole Seq; `|` with `each` distributes per element.

### Component 3: Generator Primitives

These stdlib functions produce Seqs from Dicts (the "explode" operations) and gather Seqs back:

| Function | Type | Description |
|----------|------|-------------|
| `each` | `Dict a → Seq a` | Values in insertion order; bare word in pipeline: `dict | each` |
| `each-key` | `Dict a → Seq Key` | Keys in insertion order |
| `each-kv` | `Dict a → Seq [key: Key value: a]` | Key-value pair dicts |
| `collect` | `Seq a → Dict a` | Auto-indexed dict `[0: v1 1: v2 ...]`; bare word: `seq | collect` |
| `collect-kv` | `Seq [key: Key value: a] → Dict a` | Reconstruct keyed dict from `each-kv` pairs |
| `get` | `Key → Record → Any` | Field accessor, curried: `[get "name"]`, `[get $key]`, `[get 0]` |

**Note on `empty`.** jq's `empty` (produce zero values for filtering) is intentionally excluded. In tinct, `[]` (empty dict) is the Seq terminator — there is no distinct "zero-value generator" value. Filtering is done with `[filter pred]` instead, which is cleaner for the common case and avoids type ambiguity.

**Range and slice.** `[range 0 5]` already exists in stdlib and returns a `Seq` of integers 0–4. `[slice data 2 5]` already exists in stdlib for positional Dict slicing. Both names are correct and require no changes. The removed `$a[0..2]` syntax is replaced by `$a | [range 0 2]` for numeric range generation or `[slice a 0 2]` for positional Dict slicing.

**Projection and path drilling** (Phase 3 stdlib additions):

```tinct
[select data "name" "age"]       # sub-dict with only those keys
$data | [path "users" 0 "name"]  # deep path: chains | for each step
```

These are stdlib functions, not new syntax.

### Generator Semantics Examples

```tinct
# Extract all active user names
users | each | [filter [fn [u] u.active]] | [map _.name] | collect
# → [0: "Alice" 1: "Carol"]

# Numeric range pipeline
[range 0 5] | [map [fn [n] [* n n]]] | collect
# → [0: 0 1: 1 2: 4 3: 9 4: 16]

# Dynamic field access
[fn [data key] data | [get key]]
# Replaces: [fn [data key] data[$key]]

# Integer index access
list.0        # via extended dot
list | [get 0]  # via get — equivalent for literal keys

# Point-free field extraction in a pipeline
users | each | [get "name"] | collect
# or with explicit lambda:
users | each | [fn [u] u.name] | collect
```

### `%` Pipeline

Document-to-document `%` pipeline passes the whole output value. If a document produces a `Seq`, the next document receives the Seq as `%` and can iterate it with `% | each | ...`. No implicit generator expansion at the `%` boundary — the user opts in explicitly.

## What Would Change

### Lexer (`src/lexer.rs`)

**Current:** Emits `Token::BracketAccess` when `[` immediately follows a value (no whitespace), tracks `had_whitespace_before` and `last_significant_token` for this detection. Emits `Token::Range` (`..`) only inside bracket-access frames.

**Proposed:** Remove `Token::BracketAccess`, `Token::Range`, and the whitespace-sensitive `[` detection logic. Add `Token::Pipe` (`|`). No whitespace sensitivity needed for `|`.

**Impact:** Major simplification. The whitespace-sensitive `[` detection is the most complex part of the lexer; removing it reduces both code and cognitive overhead.

### Parser (`src/parser.rs`)

**Current:** `StackFrame::BracketAccessKey` handles parsing of `[key]` and `[start..end]` frames. Dot access handles only identifier tokens after `.`.

**Proposed:** Remove `StackFrame::BracketAccessKey`. Extend dot handler to accept `Token::Int` after `.` (producing integer-key DotAccess). Add `|` as an infix operator with left-associative handling similar to `.`.

**Impact:** Major removal (bracket/range frame), minor addition (`|` infix, integer dot).

### AST (`src/ast.rs`)

**Current:** `Expr::BracketAccess { expr, key }`, `Expr::RangeAccess { expr, start, end }`.

**Proposed:** Remove both. Add `Expr::Pipe { lhs, rhs }`. Extend `DotAccess` field representation with a `DotKey` enum: `DotKey::Ident(String)` for string keys, `DotKey::Int(i64)` for integer keys.

**Impact:** Moderate — all AST visitors, the formatter, type checker, and evaluator touch `BracketAccess`/`RangeAccess` and must be updated.

### Desugar Pass (`src/desugar.rs`)

**Current:** No pipe operator exists; the desugar pass handles `$_` lambda wrapping.

**Proposed:** Add desugar rules for `Expr::Pipe { lhs, rhs }`:
- `Pipe(lhs, Call(f, args))` → `Call(f, args ++ [lhs])`
- `Pipe(lhs, VarRef(name))` → `Call(VarRef(name), [lhs])`
- `Pipe(lhs, other)` → `Call(other, [lhs])`

Extend `is_direct_underscore` to cover `Pipe { lhs, .. }` by recursing into lhs — so `$_ | f | g` correctly wraps the outermost pipe. `$_` in RHS call arguments wraps that argument as usual.

`Expr::Pipe` nodes are fully eliminated before evaluation — the evaluator and type checker see only the desugared `Call` nodes.

**Impact:** Minor addition to the desugar pass. No new continuations, no runtime dispatch.

### Evaluator (`src/eval_materialize.rs`, `src/eval_access.rs`)

**Current:** `Cont::BracketForceTarget` continuation, `eval_range_access()`, `key_in_range()`.

**Proposed:** Remove those. Extend `DotAccessForce` to handle `Key::Int` lookup when the field is an integer literal (from `DotKey::Int(n)` in the AST).

**Impact:** Removal of bracket/range paths; minor extension of DotAccess for integer keys. No new continuations needed — `Pipe` is desugared before eval.

### Type Checker (`src/typecheck.rs`)

**Current:** `check_dot_access()`, `check_bracket_access()`, `check_range_access()`.

**Proposed:** Remove `check_bracket_access()` and `check_range_access()`. No `check_pipe()` is needed — `Expr::Pipe` is eliminated in the desugar pass before type checking. The type checker sees only `Call` nodes, which are handled by the existing `check_call()`. Extend `check_dot_access()` to handle integer-key dot access (`DotKey::Int(n)`).

**Impact:** Minor — removal of two check functions, minor extension of dot access checking.

### Standard Library (`stdlib/prelude.llt`)

**Current:** `->` threading function. No `each`, `each-key`, `each-kv`, `collect-kv`, or `builtin-get`.

**Proposed:** Add `each`, `each-key`, `each-kv` as Rust builtins — required because `Value::Seq` construction is not expressible in tinct. Add `builtin-get : Key → Dict → Any` as a thin Rust primitive. Redefine `get` in prelude.llt to wrap it: `get: [fn [k xs] [builtin-get k xs]]` — `get` stays tinct-native following the same pattern as `fold: [fn [f init xs] [builtin-reduce f init xs]]`. Add `collect-kv` as a tinct stdlib function using `reduce` + `merge`. `->` documentation notes `|` as the preferred idiom for inline pipelines. Phase 3 adds `select` and `path`. `where` is omitted — use existing `filter` instead.

**Impact:** Three Rust builtins (`each`, `each-key`, `each-kv`) and one Rust primitive (`builtin-get`); `get` and `collect-kv` stay in prelude.llt.

### CLI (`src/main.rs`)

**Current:** Top-level value is always a single Dict or scalar; emits one JSON object.

**Proposed:** If the top-level value is `Value::Seq` and `emitted = true` (from the `emit` builtin, `doc/whatif/templating.md`), force each element of the Seq to completion — this drives any `emit` calls inside the generator, discarding the element values. If the top-level is a Seq and `emitted = false`, return an error: "top-level Seq — use `| collect` for JSON array output or `emit` for text output". Single-value and Dict programs are unaffected.

**Impact:** Minor. Add Seq branch in the eval/output path.

## Phased Adoption

### Phase 1: Dot Extension + `|` Desugar

Extend dot access to integer keys (`DotKey` enum). Add `|` as a desugar-pass infix operator. Add `get` builtin. Add `each`, `each-key`, `each-kv`, `collect-kv` builtins.

What this enables:
- `list.0` for integer index access (replaces `list[0]` for literal keys)
- `dict | [get $key]` for dynamic computed field access (replaces `dict[$key]`)
- `dict | [get "name"]` as point-free field accessor (or explicit `[fn [u] u.name]`)
- Left-to-right pipelines: `users | each | [filter active?] | [map _.name] | collect`

This phase is additive — bracket access still works. Migration can happen gradually.

### Phase 2: Remove Brackets

Remove `BracketAccess`, `RangeAccess`, and `..` from lexer/parser/AST/desugar/typecheck/eval. Remove `Token::BracketAccess` and `Token::Range`. Remove the whitespace-sensitive `[` detection. Add `|` to lexer denylists. Add Seq-at-top-level error to CLI.

What this enables:
- Full generator pipeline: `users | each | [filter active?] | [map _.name] | collect`
- Grammar simplification: whitespace-sensitive `[` detection removed entirely
- Streaming text output: `users | each | [fn [u] [emit [str u.name "\n"]]]`
- Any existing `dict[$key]` usage must migrate to `dict | [get $key]` or `dict.field`

This phase is **breaking** for bracket and range access. All corpus tests and examples need updating.

### Phase 3: Projection + Path + Stdlib Migration

Add `[select data keys...]` (multi-key projection via `each-kv` + `collect-kv`) and `[path data steps...]` (chained `|` over a sequence of selectors) as stdlib functions. Migrate `->` usage in stdlib/examples/docs to `|` idiom.

What this enables:
- `$data | [select "name" "age"]` — sub-dict projection
- `$data | [path "users" 0 "name"]` — deep path drilling without chained dots
- Idiomatic tinct that reads as data pipelines throughout

### Prerequisites

- Phase 1 has no prerequisites.
- Phase 2 depends on Phase 1 (needs `|` in place before removing brackets).
- Phase 3 depends on Phase 2.

### Trigger

- **Phase 1:** When writing a tinct program where `$data[$key]` dynamic access or multi-step `[-> data f1 f2]` pipelines feel awkward — Phase 1 delivers immediate ergonomic improvement.
- **Phase 2:** When the codebase has migrated all bracket-access usages in corpus tests and stdlib — the grammar simplification and generator capability become available together.
- **Phase 3:** When generator pipelines are established and `select`/`path` come up repeatedly as useful patterns.

## References

- jq manual (Tainaka et al., ongoing). "jq manual." *jq project.* — precedent for `select`/`path` projection primitives and `.[]` as an explicit explode step (tinct's `each`). Note: jq's implicit generator semantics (auto-flatMap via `|`) were considered and rejected; tinct's `|` is desugar-only with no Seq distribution.
- Meijer, E., Fokkinga, M., and Paterson, R. (1991). "Functional programming with bananas, lenses, envelopes and barbed wire." *FPCA '91.* — lens/access algebra foundations; structural motivation for composable field accessors.
- Nix manual (NixOS contributors). "Nix expression language." *nixos.org.* — `lib.pipe` (threading function, analogous to `->` and `|`), integer attribute access (`list.0` — adopted for tinct's `DotKey::Int`).
