# Macros Cluster: Implementation Plan

Comprehensive plan for the four metaprogramming proposals and their phased
implementation. Each proposal is a separate whatif document; this plan
connects them into a coherent dependency graph, establishes the critical path,
and groups work into independently shippable phases.

**Proposals covered:**

| # | Proposal | Whatif Document |
|---|----------|----------------|
| 1 | AST Dict Schema | `ast-schema.md` |
| 2 | Tinct-Hosted Formatter | `tinct-hosted-formatter.md` |
| 3 | Quasiquoting | `quasiquoting.md` |
| 4 | Desugaring as Macros | `macros.md` |

Three additional proposals follow this cluster and form Phase D — they
depend on defmacro shipping but are not conditionally gated:

| — | Macro-Rewrite | `macro-rewrite.md` |
| — | Parse-Stage Macros | `parse-stage-macros.md` |
| — | Custom Call Aliases | `call-aliases.md` |

**Implementation position:** This cluster follows the current sprint backlog
and precedes `doc/whatif/plans/typing-cluster.md`. One sprint — `formatter-full`
(M3b) — has a forward dependency on typing-cluster **A2** (`pattern-matching-basic`);
it is deferred to land immediately after that sprint ships.

---

## 0. Cluster Acceptance Procedure

When formally accepting any proposal in this cluster, apply these steps for
**each whatif being accepted** — in addition to the proposal-specific tasks
listed in §3 and §4:

1. **Mark the whatif doc**: add `**State:** Accepted — YYYY-MM-DD` as the
   second line of `doc/whatif/<name>.md` (after the `# What If:` title).
2. **Integrate spec content**: write the design into the named `doc/*.md`
   chapter(s) listed under "Spec chapters:" for that sprint. Write in present
   tense — no "planned", "will be", or TODO references.
3. **Update `doc/whatif/index.md`**: move the proposal's entry from its
   current adoption bucket to the **Accepted** section. Add acceptance date
   as a third column.
4. **Update `doc/17-references.md`**: add any new citations the proposal
   introduces. Keep entries sorted by author.
5. **Create implementation sprints**: use the sprint task lists in §4 as the
   blueprint. Sprints go in TODO.md under the relevant `##` section.

---

## 1. Why These Features Form a Cluster

These four proposals are not independent feature requests. They form a cluster
because they collectively answer a single question: **can tinct programs
manipulate tinct programs?** Each proposal extends tinct's ability to treat
its own syntax as data, and each extension either creates or depends on a
shared primitive — `src/ast_dict.rs`.

### The Shared Primitive

Every proposal in this cluster requires the same Rust function:

```rust
pub fn ast_to_dict(file: &File, opts: AstToDictOpts) -> Value
```

...and its inverse:

```rust
pub fn dict_to_ast(v: &Value) -> Result<Expr, AstError>
```

These two functions — defined in `doc/whatif/ast-schema.md` — are the axis
around which everything else turns. The schema they implement is shared by all
consumers: the formatter program, the `[quote]` evaluator, and the macro
expander. A change to the `Expr` enum propagates to the schema, and from there
to all three consumers uniformly.

### Two Functional Groups

**Group I: Infrastructure** (proposals 1 and 2)

The AST dict schema and the tinct-hosted formatter establish the shared
infrastructure. The schema is the language; the formatter is the most demanding
consumer — it must round-trip every `Expr` variant back to parseable source. If
the schema is complete enough to drive a correct formatter, it is complete enough
for quasiquoting and macros.

The formatter also serves as the *forcing function*: it is the concrete
deliverable that justifies implementing `ast_to_dict` before quasiquoting or
macros are approved. Compact formatter modes (`--oneline`, `--nospaces`,
`--minimize`) are immediately user-visible, creating real pull for M1 without
waiting for the macros path to mature.

**Group II: Metaprogramming** (proposals 3 and 4)

Quasiquoting and macros are the payoff. Quasiquoting adds `[quote]`/`[unquote]`
— the ergonomic surface for treating code as data. Macros add `[defmacro]` —
the mechanism for user-defined syntactic transformations. Both consume the
schema established by Group I.

### Which Are Independent

The following phases have no cross-dependencies within the cluster:

- **ast-schema Phase 1** (`ast_to_dict` minimal) — no dependencies at all.
  Pure serialization of the existing `Expr` enum.
- **dict-to-ast** — extends the M1 module without modifying its callers; can
  run in parallel with M3a/M3c after M1 is done.
- **formatter-compact** — tinct code only; depends on M1 output but requires
  no evaluator changes.

### Which Have Hard Dependencies

| Phase | Hard Prerequisite |
|-------|------------------|
| formatter-compact (M2a) | ast-dict-core M1 |
| quote (M2b) | ast-dict-core M1 |
| ast-dict-source (M3a) | — extends M1; no blocking dep |
| formatter-full (M3b) | ast-dict-source M3a + **typing-cluster A2** (`pattern-matching-basic`) |
| unquote (M3c) | quote M2b |
| dict-to-ast (M4a) | ast-dict-core M1 |
| defmacro (M4b) | quote M2b + dict-to-ast M4a |
| macro-hygiene (M5a) | defmacro M4b |
| macro-integration (M5b) | macro-hygiene M5a + unquote M3c |
| formatter-configurable (in M5b) | formatter-full M3b |

### Cross-Cluster Dependency

`formatter-full` (M3b) uses `[match node.type ...]` as its core dispatch
mechanism. This requires typing-cluster **A2** (`pattern-matching-basic`) to
land first — `[match]` is a hard prerequisite for the formatter program. All
other macros-cluster sprints are independent of the typing cluster and can
land before it.

---

## 2. Dependency Graph

### Full Graph

```
PHASE M1               PHASE M2                  PHASE M3
(foundation)           (first consumers)         (source info + richer quoting)

                       ┌── formatter-compact (M2a)
ast-dict-core (M1) ───┤
                       └── quote (M2b) ───────────────── unquote (M3c)

                       ast-dict-source (M3a) ──── formatter-full (M3b)
                                                  [NEEDS: typing-cluster A2]

PHASE M4               PHASE M5
(expansion engine)     (hygiene + integration)

ast-dict-core (M1) ── dict-to-ast (M4a) ──┐
                                            ├── defmacro (M4b) ── macro-hygiene (M5a) ── macro-integration (M5b)
quote (M2b) ───────────────────────────────┘

PHASE D (follows defmacro)

defmacro (M4b) ── macro-rewrite (D1) ── parse-stage-macros (D2)
              ── call-aliases (D3)
```

### Critical Path

The longest dependency chain from nothing to full macros:

```
ast-dict-core (M1)
  → quote (M2b) + dict-to-ast (M4a) [parallel]
    → defmacro (M4b)
      → macro-hygiene (M5a)
        → macro-integration (M5b)
```

Five sequential steps. Each is independently testable and shippable.

The formatter-full branch runs on a parallel track that reconverges in M5b:

```
ast-dict-core (M1)
  → ast-dict-source (M3a)
    → [wait for typing-cluster A2]
      → formatter-full (M3b)
        → formatter-configurable (part of M5b)
```

### Topological Ordering

A valid total order respecting all dependencies:

1. ast-dict-core (M1)
2. formatter-compact (M2a) + quote (M2b) + ast-dict-source (M3a) [all parallel after M1]
3. unquote (M3c) + dict-to-ast (M4a) [parallel after M2b]
4. defmacro (M4b) [needs M2b + M4a]
5. — *typing-cluster A2 interleaves here* —
6. formatter-full (M3b) [needs M3a + typing-cluster A2]
7. macro-hygiene (M5a) [needs M4b; independent of typing-cluster]
8. macro-integration (M5b) [needs M5a + M3c; formatter-configurable needs M3b]

---

## 3. Recommended Phased Implementation Order

### Phase M1: Schema Foundation

**M1. AST Dict Schema + `ast_to_dict` Minimal Mode** (`ast-schema.md`)

Implement `src/ast_dict.rs` with `ast_to_dict(None, None)` and
`ast_to_dict_expr`. This covers every `Expr` variant plus supporting types
(`Entry`, `Param`, `Annotation`, `Document`, `File`) per the canonical schema
in `doc/whatif/ast-schema.md`. No source info, no comment embedding — the
minimal mode that unblocks all first consumers simultaneously.

- **Scope:** New file `src/ast_dict.rs`, ~300 lines. No changes to existing
  code. Produces `Value::Dict` matching the schema: string `type:` discriminator
  on every node, `span:` on every node, `schema-version: 1` on root.
- **Formal model:** Pure serialization. The output is a structurally valid
  tinct dict — no new evaluation semantics.
- **Risk:** None. `ast_to_dict` is a new function with no callers yet. Every
  `Expr` match arm produces a dict; the Rust compiler enforces exhaustiveness.
- **Unlocks:** formatter-compact (M2a) and quote (M2b) simultaneously.

### Phase M2: First Consumers (deliver value immediately)

**M2a. Compact Formatter Modes** (`tinct-hosted-formatter.md` Phase 1)

Write `stdlib/formatter/compact.llt` and wire `tinct fmt --oneline`,
`--nospaces`, `--minimize` to evaluate it with the AST dict as `%`.

- **Scope:** New `stdlib/formatter/` directory and `compact.llt`. CLI change
  in `src/main.rs` to call `ast_to_dict(None, None)` and evaluate the compact
  program. Existing Rust formatter unchanged and retained for all other uses.
- **Risk:** Low. The tinct program is pure string-manipulation; the only Rust
  change is the CLI dispatch. Compact modes have no layout decisions —
  mechanical node → string translations, easily validated for round-trippability.
- **Unlocks:** Immediately user-visible. Establishes `stdlib/formatter/` as the
  home for all future formatter programs.

**M2b. `quote` Special Form** (`quasiquoting.md` Phase 2)

Add `quote` to the keyword denylist. Parse `[quote expr]` as `Expr::Quote`.
The evaluator converts the quoted expression to its dict representation via
`ast_to_dict_expr`.

- **Scope:** `quote` in denylist. `Expr::Quote(Box<Spanned<Expr>>)` variant.
  Evaluator: `Expr::Quote` → `ast_to_dict_expr(inner, AstToDictOpts::minimal())`
  → return `Value::Dict`. Type checker: `Quote → Dict`. Formatter: `[quote ...]`
  round-trip. No `unquote` yet — Phase 2 quotes are opaque.
- **Risk:** Low. One new keyword, one new AST variant. The evaluator case is a
  direct call to `ast_to_dict_expr`.
- **Unlocks:** Runtime AST inspection. Foundation for unquote (M3c) and
  defmacro (M4b).

### Phase M3: Source Info + Richer Quoting

**M3a. AST Dict Source Info + Comments** (`ast-schema.md` Phase 2)

Extend `ast_to_dict` to accept `source: Some(...)` and `comments: Some(...)`.
Enables `bare:` on string literals and `leading-comments:`, `trailing-comment:`,
`blank-before:` on entry and document nodes.

- **Scope:** `src/ast_dict.rs` additions (~100 lines). No changes to existing
  callers — `ast_to_dict(None, None)` continues to work unchanged. Additive only.
- **Risk:** None.
- **Unlocks:** formatter-full (M3b).

**M3b. Full Tinct Formatter** (`tinct-hosted-formatter.md` Phase 2)

*(deferred until typing-cluster A2 `pattern-matching-basic` lands)*

Write `stdlib/formatter/format.llt` with layout decisions via speculative
rendering and comment preservation. Wire `tinct fmt` (no mode flag) to
evaluate it; retain Rust formatter as `format_source_rust()` for LSP.

- **Scope:** `stdlib/formatter/format.llt` (~300 lines). `format-node` dispatch
  on `node.type` using `[match ...]`. `fits-inline?` via speculative
  `render-inline`. Comment and blank-line preservation using embedded metadata.
  Existing formatter corpus tests must pass against the tinct implementation.
- **Formal model:** Oppen (1980) line-breaking algorithm and Wadler (2003)
  "prettier printer" `group` semantics — tinct's binary single-line/block
  decision is a simplified subset sufficient for tinct's relatively flat structure.
- **Risk:** Moderate. Largest tinct program written to date. Correctness
  validated by running existing formatter corpus against the tinct implementation.
- **Cross-dep:** Requires typing-cluster **A2** + **A1** (let-binding, for
  `let: [inline-render: [render-inline node]]` memoization). Deferred accordingly.
- **Unlocks:** Eliminates formatter drift. User-configurable layout policy in M5b.

**M3c. `unquote` and `unquote-splice`** (`quasiquoting.md` Phase 3)

Add `unquote` and `unquote-splice` keywords. Valid only inside `[quote ...]`.
Parser tracks nesting depth; `unquote` evaluates at depth 1 only (Bawden 1999).

- **Scope:** Two new denylist entries. `Expr::Unquote`, `Expr::UnquoteSplice`
  variants. Contextual parsing: inside `[quote ...]` (depth 1), `[unquote ...]`
  switches to expression mode. Nested `[quote ...]` increments depth. Evaluator:
  `Expr::Quote` updated — walk quoted AST for `Unquote` subexpressions, evaluate
  and splice results into the enclosing node's field. `UnquoteSplice` evaluates
  to `Value::Seq` and splices each element into the enclosing list position.
- **Risk:** Moderate. Contextual parsing (nesting depth tracker) adds a parsing
  mode, similar in character to pattern-matching's pattern mode.
- **Unlocks:** Ergonomic quasiquoting for macro bodies. Prerequisite for M5b
  (connecting quasiquoting to the expander).

### Phase M4: Expansion Engine

**M4a. `dict_to_ast` + `eval-ast` Builtin** (`ast-schema.md` Phase 3)

Implement `dict_to_ast(v: &Value) -> Result<Expr, AstError>` — validates a
tinct dict and converts it back to an `Expr`. Add `eval-ast` builtin that calls
`dict_to_ast` and evaluates the result in the current environment.

- **Scope:** `src/ast_dict.rs` additions (~150 lines). New `eval-ast` builtin.
  Validation: `type:` must be a known string; required fields present; `span:`
  optional (synthetic zero span if absent); unknown fields ignored (forward-compatible).
- **Risk:** Low. `dict_to_ast` is a new path with no existing callers. `eval-ast`
  is consistent with `$include` semantics — the capability model gates execution.
- **Unlocks:** Macro expander (M4b) can convert macro output back to AST.
  Runtime code generation without `[defmacro]`.

**M4b. `defmacro` + Expansion Loop** (`macros.md` Phase 2)

Add `defmacro` keyword. Parse `[defmacro name [params] body]` as
`Expr::DefMacro`. Implement the expansion loop: walk AST top-down, call
registered macro functions on quoted arguments, replace with expansion result.
Add `gensym` builtin. Enforce depth limit for termination detection.

- **Scope:** `defmacro` in denylist. `Expr::DefMacro { name, params, body }`
  variant. New `src/expand.rs` implementing `expand_macros(ast, env)`. Macro
  functions run in a **fresh `EvalContext`** (not shared with the runtime pass)
  that inherits `EvalConfig` (capability flags, `no_fs`).
  `gensym: [] -> Str` builtin. Depth limit 100 (configurable via `TINCT_MACRO_DEPTH`).
  Pipeline becomes: `parse → expand_macros → desugar → resolve → typecheck → eval`.
- **Formal model:** Dybvig, Hieb & Bruggeman (1993) procedural macro model.
  Expansion is a fixpoint: expand top-down until no macro calls remain. Blackhole
  detection (analogous to `ThunkState::InProgress`) prevents self-referential
  expansion.
- **Lazy evaluation tension:** Macro arguments are *not* evaluated — they are
  quoted (converted to AST dicts) before being passed. The expander checks the
  macro table *before* creating argument thunks. The expanded result re-enters
  normal lazy evaluation.
- **Risk:** Major. New pipeline phase. Every `Expr` match arm in the resolver,
  typechecker, and formatter must handle `Expr::DefMacro`. Error propagation
  through the expansion phase requires careful design.
- **Unlocks:** User-defined syntactic transformations. Phase D proposals.

### Phase M5: Hygiene + Integration

**M5a. Macro Hygiene + Dual-Span Error Reporting** (`macros.md` Phase 3)

Add scope sets (Flatt 2016) for variable capture prevention. Implement dual-span
tracking: macro-generated AST nodes carry both the expansion site span and the
generated site span. Implement resugaring for error messages.

- **Scope:** `ScopeId(u32)` type. Each macro invocation gets a fresh `ScopeId`;
  introduced bindings carry the definition-site scope; call-site variables carry
  the caller's scope. Distinct bindings with the same name but different `ScopeId`s
  do not capture each other. Dual-span tracking via side map (no `Expr` changes).
  Error formatter shows both spans when present. Intentional hygiene escape hatch
  (`var!` / Elixir model) is **deferred** pending real-world usage observation
  (see Gate 2).
- **Formal model:** Flatt (2016) scope sets — simpler than rename-based
  approaches (KFFD 1986). Pombrio & Krishnamurthi (2014, 2015) resugaring
  principle — expanded code should be traceable to surface syntax —
  motivates dual-span error provenance.
- **Risk:** Moderate. Scope tracking adds state to the expander traversal.
- **Unlocks:** Macros safe for library/include use. Actionable error messages
  from macro-generated code.

**M5b. Integration — Include, `_` Port, Formatter Config** (`macros.md` Phase 4 + `tinct-hosted-formatter.md` Phase 3)

Wire macro definitions across `$include` boundaries. Port `_` desugaring from
hardcoded Rust to a tinct-defined macro. Expose formatter layout policy as
named parameters.

- **Scope:**
  - `$include` ordering: included file's macro definitions registered before
    the includer's expansion phase (same ordering Racket uses for `require`).
  - `_` desugaring: replace `desugar_underscore()` Rust pass with
    `[defmacro desugar-underscore ...]` per the proof-of-concept in
    `doc/whatif/macros.md`; remove Rust pass from `src/desugar.rs`; all
    existing underscore corpus tests must pass unchanged.
  - Formatter: `max-width:` and `max-entries:` as named params with defaults
    in `stdlib/formatter/format.llt`. `tinct fmt --width 100` passes through.
    `tinct fmt --formatter path/to/my-fmt.llt` for custom override.
- **Risk:** Low–Moderate. Include ordering is well-defined. The `_` port is
  validated by the existing underscore desugaring test suite.
- **Depends on:** M5a (hygiene, for the `_` macro to be safe), M3c (unquote,
  for ergonomic macro body), M3b (formatter-full, for configurable modes).

### Phase D: Advanced (follows defmacro)

| Sprint | Slug | Est. Tasks | Depends On |
|--------|------|-----------|------------|
| Macro-Rewrite | `macro-rewrite` | 8 | M5b; typing-cluster A1 shipped as Rust first. Match (A2/A3) excluded — `Expr::Match` special form |
| Parse-Stage Macros | `parse-stage-macros` | 8 | D1 (match uses dedicated parser mode — no longer depends on match) |
| Custom Call Aliases | `call-aliases` | 4 | M4b |

---

## 4. Sprint Plan

### Phase M1: Foundation

#### M1. `ast-dict-core`

- **Sprint slug:** `ast-dict-core`
- **Estimated tasks:** 8
  1. New `src/ast_dict.rs` with `AstToDictOpts { source: Option<&str>, comments: Option<&HashMap<...>> }` (both `None` in minimal mode)
  2. `ast_to_dict_expr` covering all `Expr` variants: `Literal`, `VarRef`, `DotAccess`, `Pipe`, `Dict`, `Call`, `Fn`, `TypeAlias`, `TypeAssert`, `Annotated`, `Rest`; stub arms for `Quote`, `DefMacro` (handled in later sprints); stub arms for `BracketAccess` and `RangeAccess` (these variants still exist in `src/ast.rs` pending access-pipeline phase 2 — emit `[type: "unsupported" name: "bracket-access" span: ...]` / `"range-access"` rather than crashing; remove stubs when access-pipeline phase 2 lands)
  3. `ast_to_dict` wrapping `File → Document → expressions`; root carries `schema-version: 1`
  4. Helpers: `annotation_to_dict`, `entry_to_dict`, `param_to_dict`, `span_to_dict`
  5. `[]` for absent optional fields (consistent presence — no key omission except comment fields which are omitted when empty)
  6. Tests: every `Expr` variant round-trips through `ast_to_dict_expr`; `schema-version: 1` on root; `span:` on every node; `type:` discriminator matches expected string per variant
  7. Negative: future unknown `type:` values handled gracefully in forward-compat placeholder
  8. Verified by: `cargo test` + manual `tinct eval` on a file with `--emit json` equivalent to confirm no existing behavior changes
- **Dependencies:** None
- **Key files:** `src/ast_dict.rs` (new), `src/ast.rs` (read-only reference)
- **Spec chapters:** `doc/15-ast.md` (§AST Dict Schema — `schema-version:`, `type:` discriminator convention, `span:` on every node, `[]` for absent optionals)
- **Unlocks:** M2a (formatter-compact), M2b (quote)

### Phase M2: First Consumers

#### M2a. `formatter-compact`

- **Sprint slug:** `formatter-compact`
- **Estimated tasks:** 5
  1. `stdlib/formatter/` directory; `compact.llt` with `format-node` dispatch: each `node.type` → string; section headers as `[str "; " name " " args ...]`; dicts as `[key: value ...]` space-separated
  2. `src/main.rs`: `tinct fmt --oneline` / `--nospaces` / `--minimize` calls `ast_to_dict(file, AstToDictOpts::minimal())` then evaluates `compact.llt` with AST dict as `%`; result string written to output
  3. Rust formatter retained unchanged for `tinct fmt` (no flag) and LSP
  4. Tests: every `Expr` variant round-trips through compact formatter; output is re-parseable; idempotent (formatting twice produces same output)
  5. Tests: `--nospaces` produces no extraneous whitespace; `--oneline` produces single-line output for multi-doc files
- **Dependencies:** M1 (`ast-dict-core`)
- **Key files:** `stdlib/formatter/compact.llt` (new), `src/main.rs`
- **Spec chapters:** `doc/12-tooling.md` (§Compact Formatter Modes — `--oneline`, `--nospaces`, `--minimize`; tinct-hosted formatter interface)
- **Unlocks:** Immediate CLI feature; establishes `stdlib/formatter/` home

#### M2b. `quote`

- **Sprint slug:** `quote`
- **Estimated tasks:** 6
  1. `quote` added to keyword denylist
  2. `Expr::Quote(Box<Spanned<Expr>>)` AST variant
  3. Parser: `[quote expr]` → `Expr::Quote`
  4. Evaluator: `Expr::Quote` → `ast_to_dict_expr(inner, AstToDictOpts::minimal())` → return `Value::Dict`; no `unquote` handling (Phase 2 quotes are opaque)
  5. Type checker: `Quote → Dict`; formatter: `[quote ...]` round-trip
  6. Tests: `[quote 42]` → `[type: "literal" kind: "int" value: 42 span: ...]`; `[quote config.host]` → dot-access dict; `[quote [f x y]]` → call dict; `[type-of [quote x]]` → `"dict"`; `[quote]` as the outer form (no evaluation of inner)
- **Dependencies:** M1 (`ast-dict-core`)
- **Key files:** `src/lexer.rs`, `src/parser.rs`, `src/ast.rs`, `src/eval.rs`, `src/eval_deep.rs`, `src/eval_materialize.rs`, `src/typecheck.rs`, `src/formatter.rs`, `src/lsp/analysis.rs`, `src/lsp/document.rs`
- **Spec chapters:** `doc/02-syntax.md` (§Quote Expression — `[quote expr]` syntax), `doc/08-evaluation.md` (§Quote Semantics — opaque Phase 2, `ast_to_dict_expr` call, result type `Dict`)
- **Unlocks:** AST inspection at runtime; foundation for M3c and M4b

### Phase M3: Source Info + Richer Quoting

#### M3a. `ast-dict-source`

- **Sprint slug:** `ast-dict-source`
- **Estimated tasks:** 5
  1. `AstToDictOpts.source: Option<&str>` field; `bare: true` on `[type: "literal" kind: "str"]` nodes when first source character at token span ≠ `"`
  2. `AstToDictOpts.comments: Option<&HashMap<usize, Vec<String>>>` field; `leading-comments:` and `trailing-comment:` on `Entry` and `Document` nodes by matching token offsets to AST spans
  3. `blank-before: bool` on every `Entry` node (based on presence of blank lines before the entry in source)
  4. Comment fields **absent** (key omitted entirely) when empty — the only exception to the no-omit rule; document this in code and spec
  5. Tests: `bare: true` for bare-word strings; `leading-comments:` embedding; `trailing-comment:` inline embedding; `blank-before: true` after blank lines; both-`None` mode unchanged and its tests unaffected
- **Dependencies:** None (extends M1 module; all M1 callers use `None, None`)
- **Key files:** `src/ast_dict.rs`
- **Spec chapters:** `doc/15-ast.md` (§AST Dict Source Info — `bare:`, comment field embedding, `blank-before:` semantics, no-omit exception for empty comment fields)
- **Unlocks:** M3b (formatter-full)

#### M3b. `formatter-full`

*(deferred until typing-cluster A2 `pattern-matching-basic` + A1 `let-binding` land)*

- **Sprint slug:** `formatter-full`
- **Estimated tasks:** 8
  1. `stdlib/formatter/format.llt`: `format-node` dispatch on `node.type` using `[match ...]`
  2. `format-literal`, `format-var`, `format-dot`, `format-pipe`, `format-call`, `format-fn`, `format-dict`, `format-type-*` — one helper per major node category
  3. `fits-inline?` via speculative rendering: `[<= [entry-count node] 4]` AND `[<= [str-length [render-inline node]] 80]`; `render-inline` bound with `let:` for memoization (`$length` is dict-only and cannot be used here)
  4. `emit-comments`: leading-comments indented with `# ` prefix; trailing-comment appended after `  # `
  5. `blank-before: true` → insert blank line before entry
  6. `src/main.rs`: `tinct fmt` (no flag) evaluates `format.llt` via `ast_to_dict(src, comments)`; Rust formatter retained as `format_source_rust()` for LSP
  7. All existing formatter corpus tests pass against the tinct implementation
  8. Tests: full formatter output is idempotent; comments preserved; blank lines preserved; section headers with metadata round-trip; output is re-parseable
- **Stdlib prerequisites (must land before M3b):**
  - `str-repeat: Str -> Int -> Str` in `stdlib/prelude.llt` — one-liner using `$reduce` over `$range`; required by `emit-comments`
  - `str-length: Str -> Int` Rust builtin in `src/builtins.rs` — required by `fits-inline?`; `$length` is dict-only
- **Scope note:** formatter programs are evaluated with `stdlib/prelude.llt` loaded — the same prelude available to all tinct programs, giving access to `join`, `map`, `has?`, `str`, etc.
- **Dependencies:** M3a (`ast-dict-source`) + **typing-cluster A1** (`let-binding`) + **typing-cluster A2** (`pattern-matching-basic`)
- **Key files:** `stdlib/formatter/format.llt` (new), `src/main.rs`, `stdlib/prelude.llt` (str-repeat), `src/builtins.rs` (str-length)
- **Spec chapters:** `doc/12-tooling.md` (§Full Formatter — speculative rendering, comment preservation, `format.llt` interface, LSP fallback)
- **Unlocks:** Eliminates formatter drift; configurable layout policy in M5b

#### M3c. `unquote`

- **Sprint slug:** `unquote`
- **Estimated tasks:** 5
  1. `unquote` and `unquote-splice` added to denylist
  2. `Expr::Unquote(Box<Spanned<Expr>>)`, `Expr::UnquoteSplice(Box<Spanned<Expr>>)` AST variants
  3. Parser: nesting depth tracker; inside `[quote ...]` (depth 1), `[unquote ...]` → `Expr::Unquote`, `[unquote-splice ...]` → `Expr::UnquoteSplice`; nested `[quote ...]` increments depth; `unquote` outside `quote` is a parse error; `[unquote-splice expr]` at the **top level** of a `[quote ...]` (not in a list/args position) is a parse error — Bawden (1999) Appendix A (`qq-expand`) rejects `tag-comma-atsign?` at top level; only valid in list positions (call args, dict entries)
  4. Evaluator: `Expr::Quote` updated — walk quoted AST for `Unquote` subexpressions; evaluate `Unquote.inner` and splice the result dict into the parent node's field; `UnquoteSplice` evaluates to `Value::Seq` and splices each element into the enclosing list position
  5. Tests: `[quote [+ [unquote x] 1]]` with `x: 42` → call node with `args: [[type: "literal" value: 42] [type: "literal" value: 1]]`; `unquote-splice` splicing into args list; `unquote` outside `quote` produces parse error; `[quote [unquote-splice xs]]` (top-level splice) produces parse error; `[quote [quote [unquote x]]]` preserves depth (inner `unquote` not evaluated)
- **Dependencies:** M2b (`quote`)
- **Key files:** `src/lexer.rs`, `src/parser.rs`, `src/ast.rs`, `src/eval.rs`
- **Spec chapters:** `doc/02-syntax.md` (§Quasiquoting — `[unquote expr]` and `[unquote-splice expr]` syntax, valid-only-inside-quote rule), `doc/08-evaluation.md` (§Quasiquoting Semantics — nesting depth rule, splice evaluation, Bawden 1999 reference)
- **Unlocks:** Ergonomic macro bodies; prerequisite for M5b

### Phase M4: Expansion Engine

#### M4a. `dict-to-ast`

- **Sprint slug:** `dict-to-ast`
- **Estimated tasks:** 5
  1. `dict_to_ast(v: &Value) -> Result<Expr, AstError>` — validate `type:` key; dispatch per known string; reconstruct `Expr` variant
  2. Validation: required fields present and of the correct shape; `span:` optional (synthetic zero span if absent); unknown fields ignored (forward-compatible)
  3. `AstError { message: String, field_path: Vec<String> }` for precise error location
  4. `eval-ast` builtin: `Dict -> Any` — calls `dict_to_ast`, evaluates in current environment; errors if dict is invalid AST; obeys capability model (no new capabilities required)
  5. Tests: every known `type:` value round-trips (`ast_to_dict_expr` then `dict_to_ast` → structurally same `Expr`); unknown fields preserved through round-trip; missing `type:` key produces `AstError`; `eval-ast` executes a manually constructed call node and returns the correct value
- **Dependencies:** M1 (`ast-dict-core`)
- **Key files:** `src/ast_dict.rs`, `src/builtins.rs`
- **Spec chapters:** `doc/15-ast.md` (§dict-to-ast — validation rules, `AstError` format, forward-compat unknown field policy), `doc/11a-builtins.md` (`eval-ast: Dict -> Any`)
- **Unlocks:** M4b (defmacro); runtime code generation without `[defmacro]`

#### M4b. `defmacro`

- **Sprint slug:** `defmacro`
- **Estimated tasks:** 10
  1. `defmacro` added to keyword denylist
  2. `Expr::DefMacro { name: String, params: Vec<String>, body: Box<Spanned<Expr>> }` AST variant
  3. Parser: `[defmacro name [params] body]` → `Expr::DefMacro`
  4. `MacroEnv`: map from name to compiled macro function, threaded through the expansion pass
  5. New `src/expand.rs`: `expand_macros(ast: &mut File, env: &MacroEnv) -> Result<(), MacroError>` — top-down walk; when `Expr::Call` fn resolves to a name in `MacroEnv`, quote all arguments (call `ast_to_dict_expr` per arg), call the macro function, `dict_to_ast` the result, replace the node, recurse
  6. `DefMacro` handling: evaluate the body in a compile-time context to produce a callable; register in `MacroEnv`; remove `Expr::DefMacro` from AST after registration (transparent to typechecker)
  7. Expansion fixpoint: re-expand result until no macro calls remain; depth limit 100 (configurable via `TINCT_MACRO_DEPTH`)
  8. `gensym: [] -> Str` builtin returning a guaranteed-fresh unique name. Names have the form `:gensym:N` (colon prefix makes collision structurally impossible — `:` is forbidden in bare-word identifiers, so users cannot write `:gensym:0` in source). Names are unique but not stable across evaluation orders.
  9. Pipeline update in `src/main.rs` **and `src/lsp/document.rs`**: `parse → expand_macros → desugar → resolve → typecheck → eval`. Both entry points must run `expand_macros` — without it, `Expr::DefMacro` nodes reach the typechecker and evaluator with no handler.
  10. `expand_macros` uses a **fresh `EvalContext`** (not the runtime context) — no shared `IncludeContext` cache, no shared `MAX_EVAL_DEPTH` budget. The compile-time context inherits `EvalConfig` (including `no_fs` and capability flags) from the runtime config so that `$include` and capability guards apply equally.
  11. Termination: depth limit 100 (per macro call-site expansion count, configurable via `TINCT_MACRO_DEPTH`) plus a **total node-count cap** (100k nodes post-expansion) to prevent exponential AST blowup. A `HashSet<(file_id, byte_offset)>` tracks in-progress call sites to detect recursive expansion of the same site (the actual blackhole guard — not `InProgress` thunk state, which is unrelated). **Synthetic node tracking:** macro-generated nodes produced by `dict_to_ast` with absent `span:` receive a synthetic zero span and cannot be keyed on source position. These are tracked by assigning a fresh monotonic `SyntheticId(u64)` at `dict_to_ast` time and including it as an alternate key in the in-progress set. A macro that generates a call to itself by constructing a synthetic call node will be caught via the synthetic ID, not the byte offset.
  12. Namespace rule: macros **cannot shadow registered Rust builtins**. At registration time, if `name` matches a builtin, `expand_macros` rejects the `[defmacro]` with an error. Lookup order: builtins take precedence over macros for built-in names.
  13. Tests: `[defmacro my-when [pred body] [quote [if [unquote pred] [unquote body] []]]]` expands correctly; `gensym` produces unique names with `:` prefix unreachable from user source; infinite expansion hits depth limit with clear error; node-count blowup hits cap; `Expr::DefMacro` absent from post-expansion AST; `[defmacro str ...]` is rejected at registration; LSP diagnostics correct for files using `[defmacro]`
- **Dependencies:** M2b (`quote`) + M4a (`dict-to-ast`)
- **Key files:** `src/parser.rs`, `src/ast.rs`, `src/expand.rs` (new), `src/main.rs`, `src/lsp/document.rs`, `src/builtins.rs` (gensym), `src/eval_deep.rs`, `src/eval_materialize.rs`, `src/lsp/analysis.rs`
- **Spec chapters:** `doc/02-syntax.md` (§Defmacro — `[defmacro name [params] body]` syntax), `doc/08-evaluation.md` (§Macro Expansion Pipeline — position in pipeline, fresh EvalContext, fixpoint expansion, depth limit + node-count cap, lazy evaluation bypass, builtin namespace protection)
- **Unlocks:** User-defined syntactic transformations; Phase D proposals

### Phase M5: Hygiene + Integration

#### M5a. `macro-hygiene`

- **Sprint slug:** `macro-hygiene`
- **Estimated tasks:** 7
  1. `ScopeId(u32)` type; `ScopeMap` threaded through expander
  2. Each macro invocation gets a fresh `ScopeId`; bindings introduced by macro body carry the definition-site scope; call-site variables carry the caller's scope
  3. Name resolution: two bindings with the same string name but different `ScopeId`s are distinct (no capture). **Simplification note:** this is a subset of Flatt (2016) §3.1's full *biggest-subset* binding resolution rule, which handles recursive macro definitions and nested scopes by selecting the binding whose scope set is the biggest subset of the use-site's scope set. The simplified model here (pairwise `ScopeId` inequality) is correct for non-recursive macros and straightforward `[defmacro]` definitions. If recursive macro patterns or unusual binding contexts arise, upgrade to the full biggest-subset rule.
  4. Dual-span tracking uses a **side map** `HashMap<NodeKey, Span>` maintained by the expander, keyed on a stable node identifier (e.g., `(file_id, byte_offset)` of the original call site). The error formatter looks up this map; no changes to `Spanned<T>` wrappers or `Expr` variant fields are needed. **Honest-tags requirement (Pombrio & Krishnamurthi 2015, Theorem 2 — Abstraction):** the side map must record accurate before/after patterns — the expansion call-site span paired with the specific expansion rule that produced the node — not just a generic call-site span. This ensures error provenance chains are *faithful*: a chain that maps a generated node back to the wrong surface location violates the Abstraction theorem. In practice: the side map value should be `(macro_name, call_site_span, expansion_rule_index)` so that nested expansions produce correct chains.
  5. Error formatter: shows "in expansion of `<name>` at line N" when a macro call span is present in the side map for the reporting node; chains across nested macro expansions
  6. **No intentional hygiene escape hatch.** `var!` (or any mechanism allowing a macro to inject bindings into the caller's scope without the caller's knowledge) is deferred — it creates an unrestricted scope injection surface for library macros. Gate separately after observing real-world usage.
  7. Tests: macro introducing binding `x` does not capture caller's `x`; error in expanded code shows macro call site; nested expansion shows full provenance chain; existing macros from M4b still work correctly
- **Dependencies:** M4b (`defmacro`)
- **Key files:** `src/expand.rs`, error formatting code
- **Spec chapters:** `doc/08-evaluation.md` (§Macro Hygiene — scope sets, dual-span side map, error provenance, resugaring principle; note: intentional hygiene escape hatch deferred)
- **Unlocks:** Macros safe for cross-include use; actionable macro error messages

#### M5b. `macro-integration`

- **Sprint slug:** `macro-integration`
- **Estimated tasks:** 6
  1. Include ordering: `expand_macros` runs on included file before expanding the includer; macro definitions in `$include`d files registered in `MacroEnv` before includer's expansion starts. **Constraint: works only for statically-determinable include paths (string literal arguments to `$include`).** Dynamic paths (computed expressions) cannot be resolved at expansion time — their macros are unavailable until eval-time, after expansion has already run. Document this limitation. **Cache note:** the existing `IncludeContext` cache stores post-eval results with `DefMacro` nodes already stripped. Macro registration from included files must bypass the cache (run `expand_macros` on a fresh parse of the included file) or the cache must be extended to store `(EvalResult, MacroEnv fragment)` tuples. Decide before implementing.
  2. Port `_` desugaring: replace `desugar_underscore()` Rust pass with `[defmacro desugar-underscore ...]` per proof-of-concept in `doc/whatif/macros.md`; remove Rust pass from `src/desugar.rs`; all existing underscore corpus tests pass unchanged
  3. Formatter configurable policy: `max-width:` and `max-entries:` named params with defaults in `stdlib/formatter/format.llt`
  4. CLI: `tinct fmt --width 100 --max-entries 6` passes through to formatter program as named args
  5. CLI: `tinct fmt --formatter path/to/my-fmt.llt` uses custom formatter program
  6. Tests: included file's macros available to includer; `_` macro produces same expansion as prior Rust pass for all corpus tests; custom formatter override is used when `--formatter` specified; `--width 100` changes layout (wider single-line threshold)
- **Dependencies:** M5a (`macro-hygiene`) + M3c (`unquote`) + M3b (`formatter-full`) for tasks 3–5
- **Key files:** `src/expand.rs` (include ordering), `src/desugar.rs` (simplified), `src/main.rs`, `stdlib/formatter/format.llt`
- **Spec chapters:** `doc/08-evaluation.md` (§Include and Macro Ordering — `$include` runs macro expansion before includer), `doc/12-tooling.md` (§Formatter Configuration — `--width`, `--max-entries`, `--formatter` flag)

---

## 5. Cross-Cutting Concerns

### AST Stability Requirement

The schema becomes a public API surface the moment `[quote]` ships. Macros
written against it may break if `Expr` variants are renamed, fields are added
without defaults, or the `type:` discriminator strings change. Before shipping
M2b:

- Audit recent `src/ast.rs` changes against the schema in `doc/whatif/ast-schema.md`
- Commit to not renaming existing `Expr` variants without a schema version bump
- `schema-version: 1` on the root `File` node is the migration escape hatch
- **Resolved discrepancy:** `Expr::BracketAccess` and `Expr::RangeAccess` were removed from `src/ast.rs` in access-pipeline phase 2. The schema no longer needs stub arms for these variants.
- **Span coverage note:** `span:` on "every node" means every `Spanned<T>` wrapper, not every sub-element. `DotKey` (the field name in a dot-access expression) carries no independent span; the dot-access node's span covers the entire `target.field` expression. Do not attempt to add `span:` fields to `DotKey` in the schema.

`dict_to_ast` is deliberately permissive on unknown fields — new fields can be
added without breaking existing macros, but renamed fields silently fall back to
defaults. Prefer additive schema changes.

### `[unquote]` is Runtime, Not Expansion-Time

`[unquote expr]` evaluates its argument when the surrounding `[quote]` expression
is forced by the normal evaluator at runtime — not during the `expand_macros` pass.
The `macros.md` and `quasiquoting.md` docs both contain descriptions suggesting
expansion-time evaluation; these have been corrected. Implementers: `Expr::Quote`
is handled in the main evaluator, not in `src/expand.rs`.

### Compile-Time Evaluation Context

Macro bodies in `[defmacro]` run in a **fresh `EvalContext`** per expansion pass —
not the runtime context. This prevents:
- `IncludeContext` cache pollution (compile-time include results mixing with runtime results)
- `MAX_EVAL_DEPTH` budget erosion (recursive macros eating runtime depth)
- `gensym` counter state leaking between passes

The fresh context **inherits `EvalConfig`** from the runtime config — `no_fs`,
capability flags, and `TINCT_MACRO_DEPTH` all apply equally to macro execution.

### `$include` Macro Ordering: Static Paths Only

Macro definitions from `$include`d files are available to the includer **only when
the include path is a string literal** determinable at expansion time. Dynamic paths
(`[include [str base-dir "/lib.llt"]]`) cannot be resolved during `expand_macros`
and their macros are not registered until eval-time — which is after expansion has run.

This is a direct consequence of Flatt's (2002) *phase separation* model: compile-time
imports (macro definitions) must be resolved before expansion begins, because the
expander needs the macro table to be complete before it walks the AST. A dynamic path
whose value depends on a runtime binding cannot participate in phase separation —
it is a runtime import, not a compile-time import. This is not a limitation specific
to tinct; Racket has the same constraint (`require` paths must be module paths resolvable
at compile time, not arbitrary expressions).

Document this constraint in the spec chapter for `defmacro`, framed as a phase
separation consequence rather than an implementation shortcut.

### Pipeline Coexistence: Expander and Desugar

After M4b, the pipeline is `parse → expand_macros → desugar → resolve → typecheck → eval`.
Both `expand_macros` and `desugar.rs` coexist during M4b and M5a. M5b removes `desugar.rs`
by porting `_` to a tinct macro. During the coexistence phase:
- The `_` macro defined in M5b must **not** conflict with the still-running `desugar.rs` pass. The Rust pass is removed atomically in M5b, not incrementally.
- Any new syntactic desugaring added between M4b and M5b should go into `desugar.rs` (not a tinct macro) to avoid depending on a half-complete macro system.

### Lazy Evaluation Tension

Macro call sites bypass normal lazy evaluation: `expand_macros` runs as a pre-eval
AST pass — entirely before thunks exist. Macro arguments never enter the thunk
lifecycle. The plan's earlier framing ("the expander must check the macro table before
creating argument thunks") is misleading: there are no thunks at expansion time at all.

The expanded result re-enters normal lazy evaluation. This is not a tension to
resolve — it is the correct semantics — but it must be clearly documented and
enforced in the expander implementation.

### Error Span Handling

Without dual-span tracking, errors in macro-expanded code point to synthetic AST
nodes with zero spans — the worst debugging experience possible. M5a must be
implemented before macros are recommended for library use. The dual-span side
map (`HashMap<NodeKey, Span>`) is the mechanism; the error formatter must look
up this map when reporting errors on macro-expanded nodes.

### Formatter Speculative Rendering

`fits-inline?` in `formatter-full` renders nodes twice in the worst case. Lazy
evaluation memoizes `render-inline` when bound with `let:`:

```tinct
format-dict: [fn [node indent]
  let: [inline: [render-inline node]]
  [if [fits-inline? inline]
    inline
    [render-block node indent]]]
```

Without let binding (typing-cluster A1), `[render-inline node]` in the
condition and in the true branch are two separate thunks. This is why M3b
depends on both A1 and A2, not just A2.

### Security: `eval-ast` and Macro Namespace

`eval-ast` executes arbitrary code, subject to the same capability model as
`$include`. It does not bypass sandboxing — the `EvalConfig` (`no_fs`, capability
flags) propagates through the evaluation path. However: `eval-ast` on a dict
encoding `[include "/etc/passwd"]` *will* read that file unless `no_fs` is set.
`eval-ast` should be documented as equivalent in trust to executing arbitrary
tinct source. Macro expansion (M4b) also executes macro bodies at compile time;
the compile-time `EvalContext` inherits `EvalConfig` so capability guards apply
equally. Document in `doc/11a-builtins.md`.

**Macro namespace protection:** macros cannot shadow registered Rust builtins
(enforced at registration time). This prevents `[defmacro include ...]` from
intercepting all `$include` calls. An attempted builtin shadow is a compile-time
error from `expand_macros`.

**Expansion node-count cap:** the depth limit (100) bounds call-site expansion
depth but not total AST growth. A macro that doubles node count at each step
produces exponential AST before hitting depth 100. The 100k-node post-expansion
cap (M4b task 11) is the correct guard against this.

---

## 6. Decision Gates

### Gate 1: Compact-Only vs Full Formatter

**Decision point:** After M2a (formatter-compact) ships and M3a (source info)
is implemented.

**Question:** Is the compact formatter sufficient, or do users need the full
formatter for `tinct fmt`?

**Trigger for proceeding to M3b:** A new `Expr` variant is added that requires
updating both the Rust formatter and a tinct program — at that point the
two-update pain motivates migrating. Or: formatter drift is already a recurring
maintenance burden.

**If no trigger:** M3b stays deferred. The Rust formatter remains the default
for `tinct fmt`. The ast-dict-source work (M3a) is still useful for quasiquoting
source info; M3b is the only casualty.

### Gate 2: Hygiene Model and Escape Hatch

**Decision point:** After M4b (defmacro) ships and real macros are written.

**Question A:** Is opt-in hygiene (`gensym` convention + scope sets from M5a)
sufficient, or do users need an intentional hygiene escape hatch (`var!` or
equivalent)?

**Trigger for an escape hatch:** A concrete real-world macro requires injecting
a binding into the caller's scope and cannot be rewritten to avoid it. The
trigger must be a real case, not a hypothetical — `var!` creates an unrestricted
scope injection surface for library macros.

**If yes (escape hatch needed):** Design with explicit opt-in at the *call site*
(the caller declares which bindings may be injected), not as a unilateral macro
declaration. This prevents transitive library macros from silently shadowing
caller bindings.

**If no trigger:** Scope sets + `gensym` naming convention are sufficient. Do
not add `var!` or any equivalent.

**Question B:** Are scope sets from M5a sufficient, or do users need full
automatic hygiene?

**Trigger for upgrading:** Repeated `gensym` patterns that could have been
prevented by automatic hygiene; macro library maintainers requesting it.

**If no trigger:** M5a's scope sets are the ceiling.

### Gate 3: Port `_` Desugaring to Tinct Macro

**Decision point:** After M5a (macro-hygiene) ships and `_` semantics are
well-understood.

**Question:** Is porting `_` desugaring from Rust to a tinct macro worth the
tradeoff?

**Trigger for proceeding:** The `_` Rust pass needs modification for a new
language feature (e.g., a new node type that the DIRECT predicate must
recognize). At that point the tinct macro is easier to maintain than the Rust
pass. Or: macro-rewrite (D1) is adopted, making the `_` port a prerequisite.

**If no trigger:** Keep `_` in Rust. The Rust pass is correct and tested.

---

## 7. Implementation Calendar

A rough ordering assuming one sprint per entry, with parallelism where
dependencies allow:

```
[current sprint backlog completes]

M1:  ast-dict-core              ← first macros-cluster sprint; no deps
M2a: formatter-compact  ─┐     ← parallel with M2b and M3a after M1
M2b: quote              ─┤     ← parallel with M2a and M3a
M3a: ast-dict-source    ─┘     ← parallel with M2a and M2b; no blocking dep
M3c: unquote                   ← after M2b
M4a: dict-to-ast               ← after M1; can run parallel with M3a/M3c

── macros-cluster core complete; typing-cluster can begin ──

[typing-cluster A1: let-binding]
[typing-cluster A2: pattern-matching-basic]
M3b: formatter-full            ← after typing-cluster A1 + A2 + M3a

M4b: defmacro                  ← after M2b + M4a (independent of typing-cluster)
M5a: macro-hygiene             ← after M4b
M5b: macro-integration         ← after M5a + M3c (+ M3b for formatter config)

── typing-cluster continues ──

D1–D3 follow M5b
```

The key insight: **M1 through M4a can all land before the typing cluster starts**.
M4b (defmacro) can also land before typing-cluster since it only depends on M2b
and M4a. Only M3b (formatter-full) must wait for typing-cluster A1+A2. M5 is
the integration sprint that interleaves naturally with typing-cluster mid-run.

---

## 8. Summary: What Ships When

| Phase | What Ships | What Users Get |
|-------|-----------|----------------|
| **M1** (foundation) | `src/ast_dict.rs` — `ast_to_dict` minimal mode | The shared primitive; nothing user-visible yet |
| **M2** (first consumers) | `tinct fmt --oneline/--nospaces/--minimize`; `[quote expr]` | Compact formatter modes; runtime AST inspection |
| **M3** (source info + richer quoting) | `ast_to_dict` with source/comments; full tinct formatter; `[unquote]`/`[unquote-splice]` | Full `tinct fmt` in tinct; ergonomic template-like macro bodies |
| **M4** (expansion engine) | `dict_to_ast`; `eval-ast`; `[defmacro]`; `gensym` | User-defined syntactic transformations |
| **M5** (hygiene + integration) | Scope sets; dual-span errors; `_` ported to tinct macro; formatter config flags | Safe cross-include macros; configurable `tinct fmt --width` |
| **D** (advanced, follows defmacro) | macro-rewrite; parse-stage macros; call-aliases | Self-hosted desugaring; richer macro forms |

Each phase is independently shippable. Phases M1–M2 deliver immediate value
(compact formatter, AST inspection) with no metaprogramming exposure. M3–M4
deliver the metaprogramming system. M5 makes it production-ready.

---

## References

Papers cited in the individual whatif documents that are load-bearing for
this plan:

- Ballantyne, M., King, A. & Felleisen, M. (2020). Macros for domain-specific languages. *OOPSLA '20*.
- Bawden, A. (1999). Quasiquotation in Lisp. *PEPM '99*, pp. 4–12. ACM. — Formal nesting depth rules for `quote`/`unquote` (M3c parser).
- Bawden, A. & Rees, J. (1988). Syntactic closures. *LFP '88*, pp. 86–95. ACM. — First-class syntactic environments for controlled variable capture; a distinct hygiene mechanism from KFFD renaming.
- Clinger, W.D. & Rees, J. (1991). Macros that work. *POPL '91*, pp. 155–162. ACM. — Unified hygienic expansion combining KFFD renaming with the R4RS `syntax-rules` pattern language. Linear-time algorithm.
- Dybvig, R.K., Hieb, R. & Bruggeman, C. (1993). Syntactic abstraction in Scheme. *Lisp and Symbolic Computation*, 5(4), 295–326. — `syntax-case`: procedural power with automatic hygiene. Formal model for M4b expansion fixpoint.
- Flatt, M. (2002). Composable and compilable macros: you want it when? *ICFP '02*, pp. 72–83. ACM. — Phase separation for compile-time evaluation; M4b include ordering.
- Flatt, M. (2016). Binding as sets of scopes. *POPL '16*, pp. 705–717. ACM. — Scope sets as the M5a hygiene model.
- Kohlbecker, E., Friedman, D.P., Felleisen, M. & Duba, B. (1986). Hygienic macro expansion. *LFP '86*, pp. 151–161. ACM. — Original KFFD hygiene algorithm.
- McCarthy, J. (1960). Recursive functions of symbolic expressions and their computation by machine, Part I. *CACM*, 3(4), 184–195. — Original `quote` in LISP.
- Oppen, D. (1980). Prettyprinting. *ACM TOPLAS*, 2(4), 465–483. — Foundational line-breaking algorithm for pretty-printing; tinct's `fits-inline?` binary decision (M3b).
- Pombrio, J. & Krishnamurthi, S. (2014). Resugaring: lifting evaluation sequences through syntactic sugar. *PLDI '14*, pp. 361–371. ACM. — Establishes the resugaring principle: expanded code should be traceable to surface syntax. Motivates dual-span error provenance for M5a.
- Pombrio, J. & Krishnamurthi, S. (2015). Hygienic resugaring of compositional desugaring. *ICFP '15*, pp. 75–87. ACM. — Extends resugaring to compositional (nested) desugaring; directly applicable to nested macro expansion provenance.
- Taha, W. & Sheard, T. (2000). MetaML and multi-stage programming with explicit annotations. *Theoretical Computer Science*, 248(1–2), 211–242. — Typed code quotation; reference for future typed quasiquoting.
- Wadler, P. (2003). A prettier printer. *The Fun of Programming*, pp. 223–243. — Combinatorial pretty-printing with `group`/`nest` operators; tinct's binary single-line/block decision (M3b) uses Wadler's `group` semantics without the full document algebra.
