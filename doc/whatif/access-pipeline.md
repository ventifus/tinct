# What If: Unified Access and Generator Pipeline for tinct

**State:** Proposal

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

`|` is tinct's new infix operator — a second infix alongside `.`, but one that *subsumes* bracket access and `->`:

```
x | f
```

**Semantics — type dispatch on LHS:**

| LHS type | RHS type | Result |
|----------|----------|--------|
| `Value::Seq` | any Fn `a → b` | Seq: flatMap f over each element |
| `Value::Seq` | any Fn `a → Seq b` | Seq: flatMap, flatten one level |
| any `a` | Fn `a → b` | `b`: apply f to x |
| any `a` | String | field lookup: `x.RHS` |
| any `a` | Int | field lookup: `x[Key::Int(RHS)]` |

The last two rows — String and Int dispatch — let `|` replace dynamic bracket access:

```tinct
$data | "name"       # same as $data.name — static string key
$data | 0            # same as $data[0] — integer key
$data | $key         # dynamic: $key evaluates to String or Int at runtime
```

The Seq-aware rows give generator power: when the LHS is a Seq, `|` flatMaps the RHS over each element. This is the jq model — single values are implicit 1-element generators, so the single-value case falls out naturally as a degenerate flatMap.

**Chaining is left-associative:**

```tinct
$users | [each] | [where active?] | [fn [u] u.name] | [collect]
# parses as: (((($users | [each]) | [where active?]) | [fn [u] u.name]) | [collect])
```

**`|` inside dict entries** parses correctly without ambiguity. The iterative parser handles `|` as an infix operator within an expression; entry boundaries are detected by the `:` lookahead rule (unchanged):

```tinct
[key: $a | f   other: x]     # key gets ($a | f), other gets x
```

**`$_` desugaring.** When `$_` appears as the LHS of `|`, it triggers lambda wrapping — same rule as the existing `$_.field` desugaring. `is_direct_underscore` is extended to cover `Expr::Pipe { lhs: VarRef("_"), .. }`:

```tinct
[map $_ | "name" users]   # desugars: [map [fn [__0] __0 | "name"] users]
```

**Relation to `->`.** The existing `->` threading function (`[-> data f1 f2 f3]`) remains in the stdlib for cases where the stage list is itself a runtime value. For inline pipelines, `|` is preferred. The stdlib and documentation migrate to `|` idiom over time.

### Component 3: Generator Primitives

These stdlib functions produce Seqs from Dicts (the "explode" operations) and gather Seqs back:

| Function | Type | Description |
|----------|------|-------------|
| `[each dict]` | `Dict a → Seq a` | Values in insertion order |
| `[each-key dict]` | `Dict a → Seq Key` | Keys in insertion order |
| `[each-kv dict]` | `Dict a → Seq [key: Key value: a]` | Key-value pair dicts |
| `[where pred seq]` | `Seq a → Seq a` | Keep elements matching predicate |
| `[collect seq]` | `Seq a → Dict a` | Materialize to auto-indexed dict (`[0: v1 1: v2 ...]`) |
| `[collect-kv seq]` | `Seq [key: Key value: a] → Dict a` | Reconstruct dict from `each-kv` pairs |

**Note on `empty`.** jq's `empty` (produce zero values for filtering) is intentionally excluded. In tinct, `[]` (empty dict) is the Seq terminator — there is no distinct "zero-value generator" value. Filtering is done with `[where pred]` instead, which is cleaner for the common case and avoids type ambiguity.

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
$users | [each] | [where [fn [u] u.active]] | [fn [u] u.name] | [collect]
# → [0: "Alice" 1: "Carol"]

# Numeric range pipeline
[range 0 5] | [fn [n] [* n n]] | [collect]
# → [0: 0 1: 1 2: 4 3: 9 4: 16]

# Dynamic field access
[fn [data key] data | $key]
# Replaces: [fn [data key] data[$key]]

# Integer index access
$list | 0      # first element
$list.0        # same, via extended dot

# Stored selector (first-class Fn)
[sel: [fn [d] d | "users" | [each] | [fn [u] u.name]]
 result: [sel $data]]
```

### `%` Pipeline

Document-to-document `%` pipeline passes the whole output value. If a document produces a `Seq`, the next document receives the Seq as `%` and can iterate it with `% | [each] | ...`. No implicit generator expansion at the `%` boundary — the user opts in explicitly.

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

**Proposed:** Remove both. Add `Expr::Pipe { lhs, rhs }`. Extend `DotAccess` field representation to distinguish `DotKey::Ident(String)` vs `DotKey::Int(i64)`, or keep `field: String` and resolve at eval time.

**Impact:** Moderate — all AST visitors, the formatter, type checker, and evaluator touch `BracketAccess`/`RangeAccess` and must be updated.

### Evaluator (`src/eval_materialize.rs`, `src/eval_access.rs`)

**Current:** `Cont::BracketForceTarget` continuation, `eval_range_access()`, `key_in_range()`.

**Proposed:** Remove those. Add `Cont::PipeForce` that:
1. Materializes the LHS
2. If `Value::Seq`: constructs a lazy flatMap over tail, returns `Seq(head | rhs, tail | rhs)`
3. If `Value::String(s)`: performs `DotAccess` with key `s`
4. If `Value::Int(n)`: performs integer key lookup
5. If `Value::Function`/`Value::Builtin`: applies to LHS
6. Otherwise: type error

Extend `DotAccessForce` to handle `Key::Int` lookup (try `Key::Int(n)` when field parses as integer).

**Impact:** Moderate for Pipe addition; minor for DotAccess integer extension; major removal of bracket/range paths.

### Type Checker (`src/typecheck.rs`)

**Current:** `check_dot_access()`, `check_bracket_access()`, `check_range_access()`.

**Proposed:** Remove `check_bracket_access()` and `check_range_access()`. Add `check_pipe()` with dual-dispatch:
- LHS type `Seq a`, RHS type `a → b` → result `Seq b`
- LHS type `Seq a`, RHS type `a → Seq b` → result `Seq b` (flatMap)
- LHS type `a`, RHS type `a → b` → result `b`
- LHS is String/Int literal → treat as field access (same as `check_dot_access`)

**Impact:** Moderate. Dual-dispatch typing already exists for `map`/`filter` — `check_pipe` follows that pattern.

### Standard Library (`stdlib/prelude.llt`)

**Current:** `->` threading function. No `each`, `each-key`, `each-kv`, `where`, `collect-kv`.

**Proposed:** Add `each`, `each-key`, `each-kv`, `where`, `collect-kv` as builtins (Rust-native, since they operate on `Value::Seq` internals). Migrate `->` documentation to note `|` as the preferred idiom. Phase 3 adds `select` and `path`.

**Impact:** Minor additions to builtins.rs; no removal of existing stdlib functions.

### CLI (`src/main.rs`)

**Current:** Top-level value is always a single Dict or scalar; emits one JSON object.

**Proposed:** If the top-level materialized value is `Value::Seq`, emit one JSON line per element (newline-delimited JSON, NDJSON). This matches jq's default output for generators. Single-value programs are unaffected.

**Impact:** Minor. Add Seq branch in the eval/output path.

## Phased Adoption

### Phase 1: Dot Extension + `|` Reverse-Apply

Extend dot access to integer keys. Add `|` as an infix operator with type dispatch (String/Int → field access, Fn → apply) but *without* Seq-aware flatMap semantics yet. Add `[where pred]` to stdlib.

What this enables:
- `$list.0` for integer index access (replaces `$list[0]` for the common literal case)
- `$data | $key` for dynamic computed field access (replaces `$data[$key]`)
- Readable left-to-right pipelines: `$data | "users" | [each] | [collect]`
- `[where pred seq]` for inline filtering without `filter` + prefix nesting

This phase is additive — bracket access still works. Migration can happen gradually.

### Phase 2: Remove Brackets + Generator Dispatch

Remove `BracketAccess`, `RangeAccess`, and `..` from lexer/parser/AST/eval/typecheck. Add Seq-aware flatMap dispatch to `|`. Add `each`, `each-key`, `each-kv`, `collect-kv` builtins. Add NDJSON multi-output to CLI.

What this enables:
- Full generator pipeline: `$users | [each] | [where active?] | [fn [u] u.name] | [collect]`
- Grammar simplification: whitespace-sensitive `[` detection removed entirely
- Any existing `$a[key]` usage must migrate to `$a | $key` or `$a.key`

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

- jq manual (Tainaka et al., ongoing). "jq manual." *jq project.* — generator model, pipe flatMap semantics, `select`/`empty` primitives
- Bird, R. and Wadler, P. (1988). *Introduction to Functional Programming.* Prentice Hall. — flatMap as monadic bind for lists; relationship between single-value and list cases
- Meijer, E., Fokkinga, M., and Paterson, R. (1991). "Functional programming with bananas, lenses, envelopes and barbed wire." *FPCA '91.* — lens/access algebra foundations
- Nix manual (NixOS contributors). "Nix expression language." *nixos.org.* — `lib.pipe` (threading), integer attribute access (`list.0`)
