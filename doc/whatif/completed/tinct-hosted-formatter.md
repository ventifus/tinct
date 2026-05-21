# What If: Tinct-Hosted Formatter

**State:** Accepted — 2026-05-05

What would it take to implement `tinct fmt` as a tinct program — with Rust
responsible only for parsing and calling the evaluator, and the formatting
logic itself written in tinct?

## Current State

`tinct fmt` is implemented entirely in Rust (`src/formatter.rs`, ~800 lines).
It takes `ParseOutput` — the parsed AST plus a comment attachment map and
source string — and walks the AST to produce formatted source.

The formatter makes two kinds of decisions:

1. **Structural rendering**: what tokens to emit for each `Expr` variant
2. **Layout decisions**: single-line (≤4 entries, ≤80 chars) vs multi-line

Both are encoded in Rust match arms. Adding a new `Expr` variant requires
updating the formatter. Changing layout policy (e.g., 100-char width, 6-entry
threshold) requires recompilation.

```tinct
# Formatted by Rust today
[
  server: [
    port: 8080
    host: "localhost"
    workers: 4
  ]
  logging: [level: "info"]
]
```

### What's Missing

1. **No user-configurable layout policy.** Line width (80) and entry threshold
   (4) are Rust constants. Changing them requires recompiling tinct.
2. **No formatter plugins.** Users cannot write alternative formatters (e.g.,
   a formatter that keeps certain dicts on one line by convention).
3. **Formatter drift.** Every new `Expr` variant requires a Rust change to
   `formatter.rs`. The formatter is not a tinct program, so it cannot be
   tested with tinct's own corpus infrastructure.
4. **Quasiquoting needs the same infrastructure.** `[quote expr]` needs
   `ast_to_dict` — the same Rust function the formatter needs to pass the
   AST to a tinct program. Implementing the formatter establishes the
   infrastructure that quasiquoting and macros require.

## Why a Tinct-Hosted Formatter Matters

**Self-hosting as a capability test.** If tinct can format tinct, it can do
serious string-manipulation work. The formatter is a demanding consumer: it
must round-trip every AST node to parseable source, handle comments, respect
layout preferences, and produce idiomatic output. Passing this test validates
tinct as a general-purpose language, not just a config DSL.

**Shared infrastructure with quasiquoting and macros.** `ast_to_dict` (the
Rust function that serializes the AST to a tinct dict) is the same primitive
that `[quote expr]` calls. Implementing the formatter's compact mode in Phase 1
simultaneously delivers the infrastructure for `[quote]`. The formatter is the
forcing function that makes this infrastructure real.

**User-configurable layout.** A tinct formatter program can expose its policy
as parameters — pass `max-width: 100` or `max-entries: 6` to the program, or
override it entirely by supplying a custom `stdlib/formatter/format.llt`.

**No formatter drift.** The tinct formatter program is tested by the same
corpus infrastructure as all other tinct code. Adding a new `Expr` variant
updates `ast_to_dict` in Rust and the tinct pattern match in the program —
the type checker catches missed cases.

## Design

The formatter is a tinct program that receives the AST as `%` (a tinct dict
matching the schema in `doc/whatif/ast-schema.md`) and returns a formatted
source string. Rust does three things: parse, serialize AST to dict, call the
evaluator.

```text
source → [Rust: parse2()] → ParseOutput
       → [Rust: ast_to_dict(opts)] → Value::Dict  (the AST as tinct data)
       → [tinct: stdlib/formatter/format.llt]       (% = AST dict)
       → formatted source string
```

The formatter program returns a `String`. The CLI writes it to the output
file or stdout — the same mechanism as `-o raw` in `tinct eval`.

### The Formatter Program

The core pattern is a recursive dispatch on `node.type`:

```tinct
# stdlib/formatter/format.llt (excerpt)

format-node: [fn [node indent]
  [match node.type
    "literal"    [format-literal node]
    "var"        node.name
    "dot-access" [format-dot node indent]
    "pipe"       [format-pipe node indent]
    "dict"       [format-dict node indent]
    "call"       [format-call node indent]
    "fn"         [format-fn node indent]
    "type-assert" [format-type-assert node indent]
    # ... one arm per node type
    ]]

format-literal: [fn [node]
  [match node.kind
    "int"   [str node.value]
    "float" [str node.value]
    "bool"  [if node.value "true" "false"]
    "str"   [if node.bare node.value [str "\"" node.value "\""]]]]
```

### Width Measurement in Tinct (Phase 3)

The full formatter's single-line/multi-line decision requires measuring the
rendered width of a node. This is feasible in tinct via speculative rendering:

```tinct
# Render node assuming single-line, measure the result
fits-inline?: [fn [node max-width max-entries]
  [and [<= [entry-count node] max-entries]
       [<= [length [render-inline node]] max-width]]]

# Layout decision: render inline if it fits, block otherwise
format-dict: [fn [node indent]
  [if [fits-inline? node 80 4]
    [render-dict-inline node]
    [render-dict-block  node indent]]]
```

`render-inline` is a pure function returning a string — computing
`[length [render-inline node]]` forces the inline render once. If the node
fits inline, that rendered string is used directly. If not, `render-inline`
result is discarded and `render-block` is called. Due to lazy evaluation,
`render-inline` is computed at most once per node (memoized when bound).

This "speculative rendering" approach means nodes that don't fit inline are
rendered twice in the worst case — inline for measurement, block for output.
For a formatter this is acceptable: formatting is not performance-critical,
and in practice most nodes either always fit or never fit.

### Comment Emission (Phase 2+)

```tinct
emit-comments: [fn [comments indent]
  [join "\n"
    [map [fn [c] [str [str-repeat " " indent] "# " c]]
         comments]]]

format-entry: [fn [entry indent]
  [str
    [if [blank-before? entry] "\n" ""]
    [if [has? entry "leading-comments"] [emit-comments entry.leading-comments indent] ""]
    [str-repeat " " indent]
    [format-entry-key entry indent]
    ": "
    [format-node entry.value indent]
    [if [has? entry "trailing-comment"] [str "  # " entry.trailing-comment] ""]]]
```

### Integration with Quasiquoting and Macros

The tinct-hosted formatter establishes the three Rust primitives that
quasiquoting and macros need:

| Phase | Formatter use | Quasiquoting/macros use |
|-------|--------------|------------------------|
| 1 | `ast_to_dict(None, None)` for compact modes | `[quote expr]` calls `ast_to_dict` |
| 2 | `ast_to_dict(Some, Some)` for full formatter | — |
| 3 | Full self-hosted; width measurement in tinct | `dict_to_ast` for `[defmacro]` |

All three features share the same schema (`doc/whatif/ast-schema.md`). A
change to the schema propagates to the formatter program, `[quote]`, and
macros uniformly.

## What Would Change

### `src/ast_dict.rs` (new file)

**Current:** No AST serialization to tinct dicts.

**Proposed:** `ast_to_dict(file: &File, opts: AstToDictOpts) -> Value`.
Covers every `Expr` variant plus `Entry`, `Param`, `Annotation`, `Document`,
`File`. When `opts.comments` is provided, embeds `leading-comments`,
`trailing-comment`, and `blank-before` on `Entry` and `Document` nodes.
When `opts.source` is provided, sets `bare: true/false` on string literals.

See `doc/whatif/ast-schema.md` for the complete dict schema.

**Impact:** New file, ~300–400 lines. No changes to `Expr` or `ParseOutput`.

### `stdlib/formatter/` (new directory)

**Current:** No tinct formatter programs.

**Proposed:**

- `stdlib/formatter/compact.llt` — compact modes (`--oneline`, `--nospaces`,
  `--minimize`): mechanical rendering with `;` for section headers, no layout
  decisions, no comments
- `stdlib/formatter/format.llt` — full formatter: layout decisions via
  speculative `render-inline`, comment and blank-line preservation

**Impact:** New stdlib files. No Rust changes beyond `ast_to_dict`.

### `src/main.rs` (formatter entry point)

**Current:** `tinct fmt` calls `format_source(input)` directly (pure Rust).

**Proposed:** `tinct fmt` calls `ast_to_dict(opts)`, then evaluates the
appropriate formatter program with the AST dict as `%`. The return value
(a string) is written to the output. The existing Rust formatter remains for
use by LSP (where loading a tinct program would be too slow) and as a
fallback.

**Impact:** Moderate — new evaluation path for `tinct fmt`. The Rust formatter
stays as `format_source_rust()` for LSP and tests.

### `doc/12-tooling.md`

**Proposed:** Document the formatter program interface — how to override the
formatter with a custom program, how to pass configuration, and the schema
reference.

**Impact:** Minor — documentation only.

## Phased Adoption

### Phase 1: Compact Modes in Tinct

Implement `ast_to_dict(None, None)` (no source info, no comments). Write
`stdlib/formatter/compact.llt` for `--oneline`, `--nospaces`, `--minimize`.

This phase simultaneously delivers `ast_to_dict` as the infrastructure for
`[quote expr]` in quasiquoting.

- `src/ast_dict.rs`: `ast_to_dict` covering all `Expr` variants; `schema-version: 1` on root
- `stdlib/formatter/compact.llt`: dispatches on `node.type` using `cond` chains (`[= node.type "literal"]` etc.) — `[match]` is not yet available at Phase 1; `[str "; " ...]` for section headers; no width decisions; no comments
- `src/main.rs`: `tinct fmt --oneline/--nospaces/--minimize` evaluates compact.llt with AST dict as `%`; result string written to output
- Tests: every `Expr` variant round-trips through compact formatter; output is re-parseable; idempotent

### Phase 2: Full Formatter in Tinct

Add `opts.source` and `opts.comments` to `ast_to_dict`. Write
`stdlib/formatter/format.llt` with layout decisions via speculative rendering
and comment preservation.

- `src/ast_dict.rs`: `bare:` on str literals; `leading-comments`, `trailing-comment`, `blank-before` on entries and documents
- `stdlib/formatter/format.llt`: full recursive formatter; `fits-inline?` via `[str-length [render-inline node]]` (requires new `str-length: Str -> Int` Rust builtin — `$length` is dict-only); `emit-comments` via `[str-repeat " " indent]` (requires `str-repeat` in `stdlib/prelude.llt`); `blank-before` blank line insertion
- `src/main.rs`: `tinct fmt` (no mode flag) evaluates format.llt; Rust formatter retained as `format_source_rust()` for LSP
- Tests: full formatter produces idempotent output; comments preserved; blank lines preserved; section headers with metadata round-trip; all existing formatter corpus tests pass

### Phase 3: Configurable Layout Policy

Expose layout policy as parameters to the formatter program. Move policy
constants (`max-width: 80`, `max-entries: 4`) from Rust to tinct.

- `stdlib/formatter/format.llt`: accept `max-width:` and `max-entries:` as named parameters with defaults
- `tinct fmt --width 100 --max-entries 6` passes through to the formatter program
- Custom formatter override: `tinct fmt --formatter path/to/my-fmt.llt`
- Tests: non-default width produces different layout; custom formatter is used when specified

### Prerequisites

- `doc/whatif/ast-schema.md` (canonical schema) — defined ✓
- `access-pipeline` Phase 1 accepted and underway — `DotKey::Int`, `Pipe` in AST ✓
- Pattern matching (`doc/whatif/pattern-matching.md`) Phase 2 for `[match node.type ...]` — the formatter program is the first large real-world user of `match` (Phase 2 only; Phase 1 compact modes use `cond` chains)
- `str-repeat: Str -> Int -> Str` in `stdlib/prelude.llt` — required by `emit-comments` for indentation; implement as a pure-tinct one-liner using `$reduce` over `$range` (Phase 2 prerequisite)
- `str-length: Str -> Int` Rust builtin in `src/builtins.rs` — required by `fits-inline?`; `$length` is dict-only and cannot be retargeted without breaking existing behavior (Phase 2 prerequisite)

### Trigger

Phase 1: When `--oneline`/`--nospaces`/`--minimize` formatter modes are
wanted AND quasiquoting is being worked on — delivering both simultaneously
from one `ast_to_dict` implementation.

Phase 2: When the Rust formatter requires changes for a new language feature
(new `Expr` variant) — at that point migrating to the tinct formatter avoids
a two-file update.

Phase 3: When users request non-default layout policy or custom formatters.

## References

- tinct `doc/whatif/ast-schema.md` — canonical AST dict schema used by this formatter, `[quote]`, and macros
- tinct `doc/whatif/quasiquoting.md` — `[quote expr]` uses `ast_to_dict`; Phase 1 of the formatter simultaneously delivers Phase 1 of quasiquoting
- tinct `doc/whatif/macros.md` — `dict_to_ast` (Phase 3 of ast-schema) enables `[defmacro]`
- tinct `doc/whatif/pattern-matching.md` — `[match node.type ...]` is the core dispatch mechanism in the formatter program
- Oppen, D. (1980). "Prettyprinting." *ACM TOPLAS*, 2(4), 465–483. — The foundational algorithm for line-breaking decisions in pretty-printing: scan tokens left-to-right, decide whether a group fits on the current line or must break. Tinct's `fits-inline?` binary decision (try single-line rendering, fall back to block layout if too wide) is a direct application of Oppen's model.
- Wadler, P. (2003). "A prettier printer." *The Fun of Programming*, pp. 223–243. — Combinatorial pretty-printing with `group`/`nest` operators; extends Oppen's line-breaking decision with a composable document algebra. Tinct's binary single-line/block decision uses Wadler's `group` semantics (flatten if it fits, break otherwise) without the full document algebra, sufficient for tinct's relatively flat structure.
- Pombrio, J. & Krishnamurthi, S. (2014). "Resugaring: lifting evaluation sequences through syntactic sugar." In *PLDI '14*, pp. 361–371. ACM. — Establishes the principle that expanded code should be traceable to surface syntax; motivates span attribution on macro-generated AST nodes
- Pombrio, J. & Krishnamurthi, S. (2015). "Hygienic resugaring of compositional desugaring." In *ICFP '15*, pp. 75–87. ACM. — Extends resugaring to compositional (nested) desugaring; directly applicable to nested macro expansion provenance
