# Unified Access and Generator Pipeline

## Overview

tinct's access and transformation model is built on two complementary
mechanisms: unified dot access (extended to integer keys) and the `|` pipe
operator. Together they replace bracket-access syntax with a left-to-right
pipeline model and generator-native data transformation.

tinct is a general-purpose programming language that puts structured data first.
The access-pipeline feature delivers:

1. **Left-to-right pipelines.** `$users | [each] | [where active?] | [fn [u] u.name] | [collect]`
   reads in execution order. The prefix `[map [fn [u] u.name] [filter active? users]]`
   reads inside-out.

2. **Generator-native iteration.** `Value::Seq` is the natural carrier.
   `dict | each` explodes a dict into a Seq; `seq | collect` gathers it back.
   Generator behavior is opt-in via explicit `each`/`each-key`/`each-kv` primitives.

3. **Uniform field access.** Static (`$data.name`) and dynamic
   (`$data | [get $key]`) feel like the same operation with different
   key-expression syntax.

4. **Grammar simplification.** Removing bracket access eliminates the
   whitespace-sensitive `[` lexer complexity — the hardest part of the prior
   parser to understand and maintain.

## Design

The redesign has three components: extend dot access, add the `|` operator, and
add generator primitives.

### Component 1: Unified Dot Access

Dot access is the *only* access syntax, extended to handle all key types:

```tinct
$data.name          # string key (existing)
$data.0             # integer key (new — previously required $data[0])
$data.0.name        # chain: integer then string
```

When the token after `.` is an integer literal, the evaluator looks up
`Key::Int(n)` instead of `Key::String("0")`. This matches Nix's behavior where
`list.0` accesses the first element.

Bracket access (`$a["key"]`, `$a[0]`, `$a[$key]`) and range access (`$a[0..5]`)
are removed. The tokens `BracketAccess` and `Range` (`..`) are removed from the
lexer along with the whitespace-sensitive `[` detection. The AST nodes
`BracketAccess` and `RangeAccess` are removed. The `BracketAccessKey` parser
frame is removed.

### Component 2: The `|` Pipe Operator

`|` is tinct's second infix operator alongside `.`. It is eliminated entirely
in the desugar pass — `lhs | rhs` is rewritten to a function call before
evaluation. No runtime dispatch occurs.

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

**The N-1 rule.** Arity checking enforces the constraint automatically.
`[map fn]` standalone is an arity error — `map` needs two arguments.
`users | [map fn]` desugars to `[map fn users]` — a valid two-argument call.
Only the RHS of `|` gets the extra argument added.

`[f: [map fn]]` — storing `[map fn]` as a dict entry value — is an arity error
because it is evaluated standalone, not in pipe context.

**No first-class partial application.** `[map fn]` is only valid as the RHS of `|`. Partial values are not a language feature; use `[fn [x] [map fn x]]` to curry explicitly.

**Dynamic field access: `get`.** `|` does not dispatch on String or Int RHS
values — those are type errors. Dynamic key lookup uses the `get` stdlib
function (curried, data-last):

```tinct
dict | [get "name"]      # static string key — point-free
dict | [get $key]        # dynamic computed key
dict | [get 0]           # integer index
dict | [fn [u] u.name]   # or explicit lambda — programmer's choice
```

`get : Key → Record → Any` returns a function `Record → Any` when given one
argument, composing naturally with `|`.

**Call-head position and `$`.** A bare word is in *call-head position* only as
the first token of a `[...]` bracket form. Everywhere else — including the RHS
of `|` — bare words are variable references, and `$` is unnecessary:

| Context | Bare word | `$`-prefixed |
|---------|-----------|--------------|
| First token of `[...]` | call-head (function to call) | VarRef (forced reference) |
| Anywhere else | VarRef | VarRef (redundant `$`) |

```tinct
users | each | [map fn] | collect   # bare words after | are VarRefs — no $ needed
[f [$x | g]]                         # $ needed: x in call-head position of inner [...]
```

**Precedence:** `.` > call > `|`. `|` terminates call argument accumulation
inside `[...]`:

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

**`|` inside dict entries** parses correctly. The `:` lookahead for entry
boundaries is unaffected:

```tinct
[key: a | f   other: x]     # key gets (a | f), other gets x
```

**`$_` desugaring.** `$_` on the LHS of `|` (or a DIRECT chain from `$_` as
LHS) triggers lambda wrapping — the whole pipe becomes `[fn [_] $_ | f]`.
`is_direct_underscore` is extended to cover `Pipe { lhs, .. }` by recursing
into lhs. `$_` in RHS call arguments wraps that specific argument as in any
other call; it does not wrap the whole pipe. Useful RHS patterns:

- Bare word: `... | each`, `... | collect`
- Field shorthand: `... | _.name` (desugars to `[fn [__0] __0.name]`)
- Explicit lambda: `... | [fn [u] u.name]`
- Partial call: `... | [map _.name]`, `... | [get "name"]`

**No implicit Seq distribution.** `|` applies its RHS to the LHS value as a
whole — there is no automatic element-wise distribution over Seqs.
`users | [length]` gives the length of the Seq. Use `each` to iterate
explicitly.

**Relation to `->`.** `->` (stdlib threading) remains for cases where the stage
list is a runtime value. The two differ for Seq values: `->` applies each stage
to the whole Seq; `|` with `each` distributes per element.

### Component 3: Generator Primitives

These stdlib functions produce Seqs from Dicts (the "explode" operations) and
gather Seqs back:

| Function | Type | Description |
|----------|------|-------------|
| `each` | `Dict a → Seq a` | Values in insertion order; bare word in pipeline: `dict | each` |
| `each-key` | `Dict a → Seq Key` | Keys in insertion order |
| `each-kv` | `Dict a → Seq [key: Key value: a]` | Key-value pair dicts |
| `collect` | `Seq a → Dict a` | Auto-indexed dict `[0: v1 1: v2 ...]`; bare word: `seq | collect` |
| `collect-kv` | `Seq [key: Key value: a] → Dict a` | Reconstruct keyed dict from `each-kv` pairs |
| `get` | `Key → Record → Any` | Field accessor, curried: `[get "name"]`, `[get $key]`, `[get 0]` |

**Note on `empty`.** jq's `empty` (produce zero values for filtering) is
intentionally excluded. In tinct, `[]` (empty dict) is the Seq terminator —
there is no distinct "zero-value generator" value. Filtering is done with
`[filter pred]` instead, which is cleaner for the common case and avoids type
ambiguity.

**Range and slice.** `[range 0 5]` already exists in stdlib and returns a `Seq`
of integers 0–4. `[slice data 2 5]` already exists in stdlib for positional Dict
slicing. Both names are correct and require no changes. The removed `$a[0..2]`
syntax is replaced by `$a | [range 0 2]` for numeric range generation or
`[slice a 0 2]` for positional Dict slicing.

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

Document-to-document `%` pipeline passes the whole output value. If a document
produces a `Seq`, the next document receives the Seq as `%` and can iterate it
with `% | each | ...`. No implicit generator expansion at the `%` boundary —
the user opts in explicitly.

## Implementation

### Lexer (`src/lexer.rs`)

`Token::BracketAccess`, `Token::Range`, and the whitespace-sensitive `[`
detection logic are removed. `Token::Pipe` (`|`) is added. No whitespace
sensitivity is needed for `|`. Removing the whitespace-sensitive `[` detection
is a major simplification — it was the most complex part of the lexer.

### Parser (`src/parser.rs`)

`StackFrame::BracketAccessKey` is removed. The dot handler is extended to accept
`Token::Int` after `.` (producing integer-key DotAccess). `|` is added as an
infix operator with left-associative handling similar to `.`.

### AST (`src/ast.rs`)

`Expr::BracketAccess { expr, key }` and `Expr::RangeAccess { expr, start, end }`
are removed. `Expr::Pipe { lhs, rhs }` is added. `DotAccess` field
representation uses a `DotKey` enum: `DotKey::Ident(String)` for string keys,
`DotKey::Int(i64)` for integer keys.

### Desugar Pass (`src/desugar.rs`)

Desugar rules for `Expr::Pipe { lhs, rhs }`:
- `Pipe(lhs, Call(f, args))` → `Call(f, args ++ [lhs])`
- `Pipe(lhs, VarRef(name))` → `Call(VarRef(name), [lhs])`
- `Pipe(lhs, other)` → `Call(other, [lhs])`

`is_direct_underscore` is extended to cover `Pipe { lhs, .. }` by recursing
into lhs — so `$_ | f | g` correctly wraps the outermost pipe. `$_` in RHS
call arguments wraps that argument as usual.

`Expr::Pipe` nodes are fully eliminated before evaluation — the evaluator and
type checker see only the desugared `Call` nodes.

### Evaluator (`src/eval_materialize.rs`, `src/eval_access.rs`)

`Cont::BracketForceTarget` continuation, `eval_range_access()`, and
`key_in_range()` are removed. `DotAccessForce` is extended to handle
`Key::Int` lookup when the field is an integer literal (from `DotKey::Int(n)`
in the AST). No new continuations needed — `Pipe` is desugared before eval.

### Type Checker (`src/typecheck.rs`)

`check_bracket_access()` and `check_range_access()` are removed. No
`check_pipe()` is needed — `Expr::Pipe` is eliminated in the desugar pass
before type checking. The type checker sees only `Call` nodes handled by the
existing `check_call()`. `check_dot_access()` is extended to handle
integer-key dot access (`DotKey::Int(n)`).

### Standard Library (`stdlib/prelude.llt`)

`each`, `each-key`, `each-kv` are Rust builtins — required because
`Value::Seq` construction is not expressible in tinct. `builtin-get : Key → Dict → Any`
is a thin Rust primitive. `get` in prelude.llt wraps it:
`get: [fn [k xs] [builtin-get k xs]]` — `get` stays tinct-native following the
same pattern as `fold: [fn [f init xs] [builtin-reduce f init xs]]`.
`collect-kv` is a tinct stdlib function using `reduce` + `merge`. `->`
documentation notes `|` as the preferred idiom for inline pipelines.

### CLI (`src/main.rs`)

If the top-level value is `Value::Seq` and `emitted = true` (from the `emit`
builtin), each element of the Seq is forced to completion — this drives any
`emit` calls inside the generator, discarding the element values. If the
top-level is a Seq and `emitted = false`, the result is an error: "top-level
Seq — use `| collect` for JSON array output or `emit` for text output".
Single-value and Dict programs are unaffected.

## References

- jq manual (Tainaka et al., ongoing). "jq manual." *jq project.* — precedent for `select`/`path` projection primitives and `.[]` as an explicit explode step (tinct's `each`). Note: jq's implicit generator semantics (auto-flatMap via `|`) were considered and rejected; tinct's `|` is desugar-only with no Seq distribution.
- Meijer, E., Fokkinga, M., and Paterson, R. (1991). "Functional programming with bananas, lenses, envelopes and barbed wire." *FPCA '91.* — lens/access algebra foundations; structural motivation for composable field accessors.
- Nix manual (NixOS contributors). "Nix expression language." *nixos.org.* — `lib.pipe` (threading function, analogous to `->` and `|`), integer attribute access (`list.0` — adopted for tinct's `DotKey::Int`).
