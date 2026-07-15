# Tinct-Hosted Formatter

## Overview

`tinct fmt` is implemented as a tinct program — with Rust responsible only for
parsing and calling the evaluator, and the formatting logic itself written in
tinct.

`formatter-compact` is complete (2026-05-06).
`formatter-full` (layout decisions with comment preservation) depends on
typing-cluster work (A1+A2).

The formatter validates tinct as a general-purpose language, not just a config
DSL. If tinct can format tinct, it handles serious string-manipulation work: the
formatter must round-trip every AST node to parseable source, handle comments,
respect layout preferences, and produce idiomatic output.

Additional benefits:

- **Shared infrastructure with quasiquoting and macros.** `ast_to_dict` (the
  Rust function that serializes the AST to a tinct dict) is the same primitive
  that `[quote expr]` calls. The compact formatter simultaneously delivers the
  infrastructure for `[quote]`.
- **User-configurable layout.** A tinct formatter program exposes its policy as
  parameters — pass `max-width: 100` or `max-entries: 6` to the program, or
  override it entirely by supplying a custom `stdlib/formatter/format.llt`.
- **No formatter drift.** The tinct formatter program is tested by the same
  corpus infrastructure as all other tinct code. Adding a new `Expr` variant
  updates `ast_to_dict` in Rust and the tinct pattern match in the program —
  the type checker catches missed cases.

## Design

The formatter is a tinct program that receives the AST as `%` (a tinct dict
matching the schema in `doc/whatif/ast-schema.md`) and returns a formatted
source string. Rust does three things: parse, serialize AST to dict, call the
evaluator.

```text
source → [Rust: parse()] → ParseOutput
       → [Rust: ast_to_dict(opts)] → Value::Dict  (the AST as tinct data)
       → [tinct: stdlib/formatter/format.llt]       (% = AST dict)
       → formatted source string
```

The formatter program returns a `String`. The CLI writes it to the output file
or stdout — the same mechanism as `-o raw` in `tinct eval`.

### The Formatter Program

The core pattern is a recursive dispatch on `node.type`:

```tinct
# stdlib/formatter/format.llt (excerpt)

format-node: [fn [node indent]
  [match node.type
    "literal":    [format-literal node]
    "var":        node.name
    "dot-access": [format-dot node indent]
    "pipe":       [format-pipe node indent]
    "dict":       [format-dict node indent]
    "call":       [format-call node indent]
    "fn":         [format-fn node indent]
    "type-assert": [format-type-assert node indent]
    # ... one arm per node type
    ]]

format-literal: [fn [node]
  [match node.kind
    "int":   [str node.value]
    "float": [str node.value]
    "bool":  [if node.value "true" "false"]
    "str":   [if node.bare node.value [str "\"" node.value "\""]]]]
```

### Compact Modes (`stdlib/formatter/compact.llt`)

The compact formatter handles `--oneline`, `--nospaces`, and `--minimize` modes.
It performs mechanical rendering with `;` for section headers and makes no
layout decisions and preserves no comments. It uses `cond` chains
(`[= node.type "literal"]` etc.) for dispatch — `[match]` will be the
preferred idiom for the full formatter once pattern matching is complete.

### Width Measurement in Tinct (full formatter)

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
`[length [render-inline node]]` forces the inline render once. If the node fits
inline, that rendered string is used directly. If not, `render-inline` result
is discarded and `render-block` is called. Due to lazy evaluation,
`render-inline` is computed at most once per node (memoized when bound).

This "speculative rendering" approach means nodes that don't fit inline are
rendered twice in the worst case — inline for measurement, block for output.
For a formatter this is acceptable: formatting is not performance-critical, and
in practice most nodes either always fit or never fit.

### Comment Emission (full formatter)

```tinct
emit-comments: [fn [comments indent]
  [join "\n"
    [map [fn [c] [str [str-repeat indent " "] "# " c]]
         comments]]]

format-entry: [fn [entry indent]
  [str
    [if [blank-before? entry] "\n" ""]
    [if [has? "leading-comments" entry] [emit-comments entry.leading-comments indent] ""]
    [str-repeat indent " "]
    [format-entry-key entry indent]
    ": "
    [format-node entry.value indent]
    [if [has? "trailing-comment" entry] [str "  # " entry.trailing-comment] ""]]]
```

### Integration with Quasiquoting and Macros

The tinct-hosted formatter establishes the three Rust primitives that
quasiquoting and macros need:

| Phase | Formatter use | Quasiquoting/macros use |
|-------|--------------|------------------------|
| 1 | `ast_to_dict(None, None)` for compact modes | `[quote expr]` calls `ast_to_dict` |
| 2 | `ast_to_dict(Some, Some)` for full formatter | — |
| 3 | Full self-hosted; width measurement in tinct | `dict_to_ast` for `[macro]` |

All three features share the same schema (`doc/whatif/ast-schema.md`). A change
to the schema propagates to the formatter program, `[quote]`, and macros
uniformly.

## Implementation

### `src/ast_dict.rs`

`ast_to_dict(file: &File, opts: AstToDictOpts) -> Value` covers every `Expr`
variant plus `Entry`, `Param`, `Annotation`, `Document`, `File`. When
`opts.comments` is provided, it embeds `leading-comments`, `trailing-comment`,
and `blank-before` on `Entry` and `Document` nodes. When `opts.source` is
provided, it sets `bare: true/false` on string literals. Approximately 300–400
lines of Rust. No changes to `Expr` or `ParseOutput`.

See `doc/whatif/ast-schema.md` for the complete dict schema.

### `stdlib/formatter/` (new directory)

- `stdlib/formatter/compact.llt` — compact modes (`--oneline`, `--nospaces`,
  `--minimize`): mechanical rendering with `;` for section headers, no layout
  decisions, no comments. **Implemented.**
- `stdlib/formatter/format.llt` — full formatter: layout decisions via
  speculative `render-inline`, comment and blank-line preservation. Implemented
  via typing-cluster A1+A2.

### `src/main.rs` (formatter entry point)

`tinct fmt` calls `ast_to_dict(opts)`, then evaluates the appropriate formatter
program with the AST dict as `%`. The return value (a string) is written to the
output. The existing Rust formatter remains as `format_source_rust()` for LSP
(where loading a tinct program would be too slow) and as a fallback.

### `doc/12-tooling.md`

Documents the formatter program interface — how to override the formatter with a
custom program, how to pass configuration, and the schema reference.

## References

- tinct `doc/whatif/ast-schema.md` — canonical AST dict schema used by this formatter, `[quote]`, and macros
- tinct `doc/whatif/quasiquoting.md` — `[quote expr]` uses `ast_to_dict`; the compact formatter simultaneously delivers the infrastructure for quasiquoting
- tinct `doc/whatif/macros.md` — `dict_to_ast` (Phase 3 of ast-schema) enables `[macro]`
- tinct `doc/whatif/pattern-matching.md` — `[match node.type ...]` is the core dispatch mechanism in the formatter program
- Oppen, D. (1980). "Prettyprinting." *ACM TOPLAS*, 2(4), 465–483. — The foundational algorithm for line-breaking decisions in pretty-printing: scan tokens left-to-right, decide whether a group fits on the current line or must break. Tinct's `fits-inline?` binary decision (try single-line rendering, fall back to block layout if too wide) is a direct application of Oppen's model.
- Wadler, P. (2003). "A prettier printer." *The Fun of Programming*, pp. 223–243. — Combinatorial pretty-printing with `group`/`nest` operators; extends Oppen's line-breaking decision with a composable document algebra. Tinct's binary single-line/block decision uses Wadler's `group` semantics (flatten if it fits, break otherwise) without the full document algebra, sufficient for tinct's relatively flat structure.
- Pombrio, J. & Krishnamurthi, S. (2014). "Resugaring: lifting evaluation sequences through syntactic sugar." In *PLDI '14*, pp. 361–371. ACM. — Establishes the principle that expanded code should be traceable to surface syntax; motivates span attribution on macro-generated AST nodes
- Pombrio, J. & Krishnamurthi, S. (2015). "Hygienic resugaring of compositional desugaring." In *ICFP '15*, pp. 75–87. ACM. — Extends resugaring to compositional (nested) desugaring; directly applicable to nested macro expansion provenance
