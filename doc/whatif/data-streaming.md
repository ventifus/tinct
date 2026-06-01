# What If: Tinct Stream Format — Stdlib-Closed Normal Form

**State:** Accepted — 2026-05-30

What would it take to give tinct a native streaming format where records carry computational structure — not just ground values — so that two tinct programs connected by a pipe remain as lazy and composable as a single program?

## Current State

tinct uses JSON as the intermediary format for structured data moving between Rust and tinct programs. The profiling pipeline illustrates the pattern:

```sh
# Collect
tinct run --profile spans.json program.llt

# Analyze — requires jq to collect the stream before tinct can read it
jq -s '.' spans.json | tinct run -i json scripts/profile/materialize.llt
```

`src/profiling.rs` writes NDJSON via `to_ndjson_line()` — a manual JSON serializer (serde was already removed by the `json-remove-serde-dep` sprint). Between tinct programs, there is no streaming mode at all: connecting two programs requires full serialization to JSON and full deserialization back, buffering the entire dataset before the downstream program sees any of it.

### What's Missing

1. **A streaming input mode.** `-i json` requires the full input to be a single JSON value. Long-running processes, pipes, and TCP connections require `jq -s '.'` to collect the full stream first.
2. **A streaming output mode.** There is no way to emit records lazily as they are produced for downstream consumption.
3. **Preservation of computational structure.** JSON serialization forces every value to a ground scalar. A filter predicate, a partially applied function, a range expression — all are reduced to their output values. The downstream program cannot see or exploit the structure of how those values were produced.
4. **Composable tinct pipelines.** `tinct run filter.llt | tinct run analyze.llt` has no efficient mode that keeps both sides lazy.
5. **A Rust-side serializer with zero dependencies.** Writing structured data from Rust currently requires `serde` + `serde_json`.

## Why Stream Format Matters for tinct

**Records carry structure, not just values.** A stream record is a tinct expression — potentially containing stdlib function calls, lazy sequences, reconstructed closures — that the consumer evaluates in their own context. Two programs connected by a stream pipe are as composable as a single program.

**Computational structure survives the pipe.** A filter predicate with an inlined threshold arrives at the consumer as `[filter [fn [let x] [> x 42]] items]` — a call they can compose lazily. JSON collapses this to a pre-computed list.

**Output formatters are just tinct programs.** `-o stream` is `stdlib/cli/out/stream.llt` — a concurrent tinct task that drains the emit channel, forces the lazy return value, and serializes both. Users write their own output formatters following the same three-step contract. No hardcoded magic in `emit` or the evaluator.

**serde_json disappears from the profiling path.** The background flush thread constructs a `Value::Dict` from each `SpanRecord` and calls `val.to_tinct(None)` — the same path as any other caller. No special fast path, no derive macros, no dependencies. `to_tinct` takes `Option<&Arc<EvalContext>>`; `None` is valid for scalar-only dicts since EvalContext is only needed for Function closure substitution.

**External tools are not required.** The profiling pipeline becomes:

```sh
tinct run --profile spans.llt-stream program.llt
tinct run -i stream scripts/profile/materialize.llt < spans.llt-stream
```

**Any readable or writable source works.** `BufRead`/`Write` are the only requirements: stdin, a file, a named pipe, or a TCP connection.

## Design

### Stdlib-Closed Normal Form

The central concept of the stream format is the **stdlib-closed normal form** (SCN) of a value.

An expression is *stdlib-closed* if every free variable in it refers to a stdlib definition — a name bound in prelude or any stdlib module. The SCN of a value V is the minimal stdlib-closed tinct expression that evaluates (in any tinct environment with the standard library) to the same value as V.

The SCN algorithm is defined by cases:

**Serializable values** — have a complete, round-trippable SCN representation:

| Value | SCN |
|-------|-----|
| `Int(n)` | `n` |
| `Float(f)` | `f` |
| `Bool(b)` | `true` / `false` |
| `String(s)` | `"s"` with standard escapes (`\\` `\"` `\n` `\t`) — always single-line quoted; `to-tinct` never emits `"""` triple-quoted strings |
| `Decimal(d)` | `d` (decimal literal) |
| `BigInt(n)` | `n` (integer literal) |
| `Bytes(b)` | `[bytes [0: b₀  1: b₁  ...]]` using the `bytes` stdlib constructor |
| `Timestamp(t)` | `[timestamp-nanos t]` — `t` is nanoseconds since UTC epoch; lossless round-trip. `timestamp-nanos` implemented in S-789/T-712 (same pattern as `duration-nanos`) |
| `Duration(d)` | `[duration-nanos d]` — exact round-trip using existing constructor; `d` is nanoseconds |
| `Dict([])` | `[]` |
| `Dict(entries)` | `[k: SCN(v)  ...]` for each entry |
| `Overlay(l, r)` | `[k: SCN(v)  ...]` — flattened to Dict before serialization |
| `Seq { head, tail }` | `[seq SCN(head) SCN(tail)]` — uses `seq` builtin constructor; `|` is the pipe operator and cannot be used here |
| `Builtin(b)` | `b.name` — builtins are always stdlib; serialize by name string |
| `Variant(tag, None)` | `tag` — nullary constructor |
| `Variant(tag, Some(payload))` | `[tag SCN(payload)]` |
| `Expression(node)` | the tinct source text of the node, produced by `fmt_expression` in `src/surface_fmt.rs` — a SurfaceNode → tinct text unparser (see note below) |

**`Expression` serialization note.** `fmt_expression(node: &Arc<SurfaceNode>) -> String` in `src/surface_fmt.rs` is a SurfaceNode → tinct source text unparser — essentially a tinct pretty-printer. It must handle all `SurfaceExpression` variants (Int, Float, Bool, Str, VarRef, Dict, Seq, Call, Fn, DotAccess, Match, Pipe, Quote, etc.) and produce tinct source text that `[load s]` can parse back to an equivalent AST. No such serializer currently exists; implementing it is a dependency of this sprint. The parse↔unparse co-location design in `src/surface_fmt.rs` directly supports this: `fmt_expression` is the unparse direction of the parser, and they should be developed together.

**Non-serializable values.** `Value::to_tinct` returns an error for any value for which it cannot construct a stdlib-closed tinct expression. This is not a blacklist — it is a consequence of the match: some arms have something to emit and some don't. Values with no tinct representation include atomic capabilities (DirCap, NetCap, ClockCap, RevocableDirCap — created by the runtime at the CLI boundary, no tinct constructor exists), values derived from capabilities (Handle, WriteHandle, QuicSession, Http2Session, Http3Session, QuicDatagramHandle, DatagramHandle — their constructors require a capability argument that is itself unexpressible as tinct source), live async runtime objects (Task, Channel, Context, Builder — opaque Rust state), opaque Rust objects (Proxy — an intercepting trapping proxy with no tinct constructor; Timezone — an opaque tz database entry), structural AST values (Program, Document — returned by `[expand [load s]]`; no tinct constructor exists that reconstructs them, and round-tripping through source text is the caller's responsibility), and `Uri` — currently an opaque Rust object with no tinct constructor (`builtin_uri` returns a `Value::Dict` of components, not `Value::Uri`; already non-serializable in the JSON path at `lib.rs:793`). `Uri` should be a tinct-native data structure rather than an opaque Rust value, but this is deferred to lib-net-v3 which will redesign the networking layer. Silently substituting `[]` for these would produce incorrect downstream behavior far from the serialization site; an immediate error is correct.

**`Function`** is serializable when the `CoreExpr` body is walkable (always the case for user-defined functions). See §Functions — complete design for the substitution and capture-avoidance algorithm. Functions produced by dynamic `eval` with no resolvable env may require conservative `[]` fallback.

**Functions — complete design.** `CoreExpr::Var` retains the original string `name` alongside de Bruijn coordinates (`level`, `slot`) — `src/ast.rs:901`. `CoreExpr::FreeVar(String)` (`src/ast.rs:907`) is the lowering fallback for names that could not be resolved to de Bruijn coordinates (include-introduced bindings, macro transformer bodies, wildcard match arm variables). Both are name-keyed references resolved against the environment at runtime. `CoreExpr::Fn` stores `params: Vec<Spanned<CoreParam>>` where each `CoreParam` also has `name: String` — `src/ast.rs:982`. The `CoreExpr` body tree is therefore fully walkable by name; no additional source-recovery infrastructure is needed.

`fmt_fn` for `Value::Function { params, body, env, .. }`:

1. **Identify non-stdlib free variables** — walk the `CoreExpr` body. For each `Var { name, .. }` or `FreeVar(name)` (treated identically — de Bruijn coordinates are irrelevant to the SCN walker): if `name` is a current-scope param name, it is a binding reference (leave as `name`); if `name` is in `stdlib_env`, it is a stdlib reference (leave as `name`); otherwise it is a captured user-env binding that must be substituted.
2. **Substitute** — for each captured name `x`, replace all `Var { name: x, .. }` and `FreeVar(x)` occurrences in the body with the inline SCN: `SCN(env.lookup(x))`. For nested `CoreExpr::Fn` nodes, extend the param scope before recursing.
3. **Capture avoidance** — before substituting, check whether any param name `p` appears free in any `SCN(env.lookup(x))`. If so, alpha-rename `p` to a fresh gensym name (`ℊꜱʏᴍ⧼p⧽N` form — prefix U+210A ℊ followed by small-caps SYM, then the original param name and a counter) in both the param list and all `Var { name: p, .. }` and `FreeVar(p)` binding references in the body. This is the Barendregt convention applied mechanically. The `ℊꜱʏᴍ` prefix characters (U+210A, U+A731, U+028F, U+1D0D) are all Unicode Letter-category and valid tinct identifier characters, so the SCN output remains parseable; a collision requires the user to deliberately type these Unicode codepoints, making accidental collision practically impossible. The `⧼`/`⧽` delimiters (U+29FC/U+29FD, Unicode category Ps/Pe — not Letter-category) are also valid in tinct identifiers: the lexer accepts all Unicode codepoints not on its explicit denylist, and these characters are not denylisted.
4. **Serialize** — emit `[fn [let params'] body']` using the substituted, possibly alpha-renamed body.

**CoreExpr traversal — complete variant table.** The free-variable walk and substitution pass must handle every `CoreExpr` variant. The walker carries a *param scope* (set of names bound by enclosing `Fn` params and `CaseArm` pattern bindings) and replaces captured references with their inline SCN.

| Variant | Walk / Substitute action |
|---------|--------------------------|
| `Int`, `Float`, `Bool`, `Str` | Leaf — nothing to do |
| `Placeholder`, `Error(_)` | Leaf — nothing to do |
| `Rest(_)` | Leaf — always a rest-parameter reference, in param scope |
| `Var { name, .. }` | **Decision point**: if `name` ∈ param scope → leave as `name`; if `name` ∈ stdlib → leave; else → substitute with `SCN(env.lookup(name))` |
| `FreeVar(name)` | Same as `Var` — de Bruijn coordinates irrelevant to SCN |
| `Annotated { name, .. }` | Same as `Var` — `name` is a variable reference in annotation position |
| `DotAccess { expr, field }` | Recurse into `expr`; `field` is a static key (no variables) |
| `Sequential(exprs)` | Recurse into each expr in order |
| `Dict(entries)` | Recurse into each entry's `key` (if `Some`) and `value` |
| `Call { func, args, named_args, .. }` | Recurse into `func`; recurse into each positional `arg`; recurse into each `named_arg.value` |
| `TypeAssert { expr, .. }` | Recurse into `expr`; annotation is a type expression, not a variable reference |
| `TypeApp { func, arg }` | Recurse into `func` and `arg` |
| `Match { scrutinee, arms }` | Recurse into `scrutinee`; recurse into each arm (each is a `CaseArm`) |
| `CaseArm { pattern, body, guard }` | Collect variable names bound by `pattern` → extend param scope → recurse into `body` with extended scope; if `guard` is `Some(expr)`, recurse into the guard expression with the extended scope (pattern variables are in scope for the guard); also recurse into `pattern` for nested closures in guard positions |
| `Fn { params, body, .. }` | Extend param scope with all param names → recurse into `body` with extended scope. Capture avoidance (step 3) must be applied before substituting into the body. |
| `PatternDecl { bindings }` | Recurse into each binding (pattern positions, may contain nested closures) |
| `LetDecl { bindings }` | Recurse into each binding |
| `Quote(expr)` | **Do not substitute** — quoted code is opaque AST data passed to macros, not evaluated in the closure's context. Recurse only into nested `Unquote`/`UnquoteSplice` sub-expressions (these ARE evaluated). Track quote depth; at depth > 0, Unquote decrements depth and resumes substitution. |
| `Unquote(expr)` | If at quote depth > 0: decrement depth, recurse into `expr` normally. At depth 0: recurse normally (Unquote outside Quote is a no-op in the surface language). |
| `UnquoteSplice(expr)` | Same as `Unquote` |

**CaseArm pattern variable extraction.** For each arm's `pattern`, collect variable names using the same rules as lowering: `Variable(name)` bindings, dict-pattern field bindings, seq-pattern head/tail bindings. These names are added to the param scope for that arm's body only.

**Mutual recursion: non-serializable.** If closure `f` captures closure `g` and `g` captures `f`, `SCN(f)` attempts to inline `SCN(g)` which attempts to inline `SCN(f)` — infinite recursion. The `InProgress` sentinel fires and produces a cycle error, the same as for cyclic value graphs. Mutually recursive closures cannot be serialized to the stream format.

No changes to `FnAnnotation` or `src/value.rs` are required.

**`src/surface_fmt.rs` addition:** `fn fmt_fn(params: &[Param], body: &Arc<Spanned<CoreExpr>>, env: &Environment, ctx: &Arc<EvalContext>) -> Result<String, String>` — implements steps 1–4. (`Param` is the type stored in `Value::Function.params: Rc<Vec<Param>>`; nested `CoreExpr::Fn` nodes inside the body use `CoreParam`, which has the same fields.) Stdlib membership check: `ctx.config.stdlib_env.read().unwrap().lookup(name).is_some()` — after builtin-privacy, `stdlib_env` is `prelude_dict` (the result of Phase 3 bootstrap), so stdlib names are those exported by prelude and any included stdlib modules.

**Cyclic values.** `to-tinct` forces eagerly. A cyclic value graph (e.g., a lazy recursive `Seq` with `xs: [cons 1 xs]`) will hit the InProgress sentinel during SCN traversal and produce a cycle error — the same behavior as `to-json`. Cyclic structures cannot be serialized to the stream format.

For an unevaluated thunk with expression E and environment env: substitute each non-stdlib free variable in E with `SCN(env.lookup(var))`. This requires forcing the binding — if a free-variable binding diverges (infinite recursion, blocking I/O), `to-tinct` will hang on that binding.

**Capture avoidance in substitution.** When substituting `SCN(env.lookup(x))` into an expression containing a lambda `[fn [let p] body]`, if `p` appears free in `SCN(env.lookup(x))`, the substitution would capture `p`. The fix is alpha-renaming: before substituting, check each lambda param `p` against the free variables of all substitution values; if a conflict exists, rename `p` to a fresh gensym name (`ℊꜱʏᴍ⧼p⧽N` form — e.g. `ℊꜱʏᴍ⧼p⧽0`, `ℊꜱʏᴍ⧼foo⧽1`) in both the binding position and all references in the body. The `ℊꜱʏᴍ` prefix (U+210A SCRIPT SMALL G + U+A731 LATIN LETTER SMALL CAPITAL S + U+028F LATIN LETTER SMALL CAPITAL Y + U+1D0D LATIN LETTER SMALL CAPITAL M) is composed of Unicode Letter-category characters and is therefore a valid tinct identifier, keeping the SCN output parseable. The `⧼`/`⧽` delimiters (U+29FC/U+29FD, Unicode Ps/Pe — not Letter-category) are also valid: tinct's lexer uses a denylist approach rather than a Unicode-category allowlist, and these codepoints are not on the denylist. A name collision requires a user to deliberately type these codepoints — accidental collision is practically impossible. This is the standard Barendregt hygiene convention applied mechanically during SCN construction.

**Gensym convention — design note.** This whatif changes the output format of the `[gensym ...]` builtin (`src/builtins_meta.rs:595`) from `:prefix:N` (e.g. `:foo:0`) to `ℊꜱʏᴍ⧼prefix⧽N` (e.g. `ℊꜱʏᴍ⧼foo⧽0`). The old `:p:N` format used `:` as a delimiter, which is a lexer-denylist character (`Token::Colon`) — correct for unforgeability but wrong when the output must be parseable source text, as it is for SCN. The new format uses the prefix `ℊꜱʏᴍ` (U+210A SCRIPT SMALL G + U+A731 LATIN LETTER SMALL CAPITAL S + U+028F LATIN LETTER SMALL CAPITAL Y + U+1D0D LATIN LETTER SMALL CAPITAL M) — all Unicode Letter-category characters and therefore valid tinct identifiers. Collision requires deliberate IME input of these codepoints.

**This migration is complete (T-711, sprint S-789).** All callers have been updated:

- `stdlib/prelude.llt` `do-desugar-inferred`: `[gensym "do-infer"]` now produces `ℊꜱʏᴍ⧼do-infer⧽N`. Both sentinel checks were updated atomically:
  - `src/eval.rs` (`CoreExpr::FreeVar` dispatch): uses `name.starts_with("ℊꜱʏᴍ⧼do-infer⧽")`
  - `src/typecheck.rs` (`check_do_infer` dispatch): same sentinel — both updated together so `do_infer_resolutions` map keys remain consistent.
- `src/eval.rs` (pipeline functions) migrated from `__nominal_input_N` to `ℊꜱʏᴍ⧼nominal-input⧽N` for consistency.
- Any macro that calls `[gensym]` or `[gensym prefix]` automatically picks up the new format with no change needed.

The `ℊꜱʏᴍ` prefix is the canonical convention for all compiler-generated identifiers in tinct going forward: macro hygiene, ANF intermediate names, CPS variables, and SCN capture-avoiding renames all use this prefix.

**Example — the filter case:**

```tinct
[
  threshold: 42
  items:     [0: 1  1: 2  2: 3  3: 4  4: 5]
]
[filter [fn [let x] [> x threshold]] items]
```

SCN computation:

- `filter` → stdlib, left as reference
- `[fn [let x] [> x threshold]]` → user closure, env has `threshold = 42`:
  - body `[> x threshold]`: `>` is stdlib (left), `x` is param (left), `threshold` → inline `42`
  - SCN: `[fn [let x] [> x 42]]`
- `items` → non-stdlib → `[0: 1  1: 2  2: 3  3: 4  4: 5]`

Result:

```tinct
[filter [fn [let x] [> x 42]] [0: 1  1: 2  2: 3  3: 4  4: 5]]
```

The consumer receives a call to `filter` they can compose, `take 10`, or pass to further analysis. `-o json` would have forced this to a pre-computed list.

### `emit` — the new prelude implementation

`emit` currently aliases `builtin-emit` directly (a Rust function that writes a String to stdout). The new implementation replaces this with a channel send:

```tinct
# stdlib/prelude.llt (replacing line 2147)
emit@[doc: "Emit a value to the output channel"]:
  [fn@Null [let v@Any] [send %emit v]]
```

`%emit` is created by `eval-programs` and injected into every program's scope as a `Channel@Any`. `emit v` sends `v` to that channel; the formatter decides how to serialize it. Call sites are agnostic to the serializer — they just `emit`.

**Strictness note.** `[send %emit v]` materializes `v` before putting it on the channel (`builtin_send` at `src/builtins_async.rs:402`). This means `emit` is a strictness point: the emitted value is fully forced at call time, not at serialization time. The structural transfer claim in the SCN section refers to the `to-tinct` output format preserving callable structure (e.g., a forced `Value::Function` or `Value::Variant`), not to thunks travelling lazily across the channel.

`eval-programs` creates a `%emit` channel and injects it into every program's scope (see §`stdlib/loader.llt`). Every program in the pipeline can call `emit v` and can receive from `%emit`. The CLI injects `%stdout` (a writable handle) into the environment.

```text
user program  ──%emit──▶  output program  ──%stdout──▶  actual stdout
 (emit producer)          (emit consumer, also handles %)
```

- Any program can call `emit v` = `[send %emit v]` — not just user programs.
- The output program (last in the pipeline) acts as the **emit consumer**: it drains `%emit` and writes to `%stdout`.
- By convention, output programs do not call `emit` themselves — they consume from `%emit` and write to `%stdout`.

**`%emit` is always available.** `%emit` is created by `eval-programs` and injected into every program's scope — always present so `emit` never fails with "undefined variable" in any context. The default output program is `stdlib/cli/out/none.llt`: it drains `%emit` discarding all values, and forces `%` discarding all elements. Programs evaluate fully (side effects and `emit` calls fire normally) but nothing is printed. Output requires an explicit `-o` flag.

**`%` threading and `%emit` are orthogonal.** `eval-programs` threads `%` sequentially through the list — the return value of each program becomes `%` for the next. `eval-programs` also creates `%emit` once and injects it into every document's scope. The output formatter drains `%emit` concurrently within its own execution via `drain: [task ...]`.

### `to-tinct` — the SCN function

`to-tinct` is a new prelude function backed by `Value::to_tinct(ctx)` — an inherent method on `Value` in `src/value.rs` that dispatches to per-type formatters in `src/surface_fmt.rs` (expression-level: Dict, Seq, Variant, Function, Expression) and `src/lexer.rs` (token-level: Int, Float, Bool, String, Decimal, BigInt, Bytes). It takes any tinct value and returns its SCN as a String:

```tinct
[to-tinct [filter [fn [let x] [> x 42]] items]]
# → "[filter [fn [let x] [> x 42]] [0: 1  1: 2  2: 3  3: 4  4: 5]]"
```

`to-tinct` is a regular stdlib function. Any tinct program or output formatter can call it directly. It is not magic — it is the serializer, exposed.

### Streaming Input — `stdlib/codecs/stream.llt`

The stream codec is implemented entirely in tinct, composing existing primitives:

- `str-chars s` — already in prelude; returns a lazy Seq of single-character strings
- `lines handle` — already in prelude; returns a lazy Seq of line strings from a readable handle
- `parse-string s` — **new builtin required** (see design note below); parses a tinct source string to `[Seq Expression]`

**Design note — `eval` does not accept String.** The `eval` builtin takes `[Seq Expression]` (AST node values), not a String (`src/builtins_meta.rs:1592`). The stream codec cannot use `[map eval ...]` directly on balanced-expr strings.

**Resolution: compose `load` + `expand` + field access — no new builtin needed.**

`load` already accepts a source **string** (the file content, not a path — `src/builtins_meta.rs:1408`). `[expand [load s]]` returns `Value::Program`. Field access on `Value::Program` and `Value::Document` is implemented in the evaluator (`src/eval_materialize.rs:2461-2533`):

- `program.documents` → `[Seq Document]` — changed from integer-keyed Dict to Seq by this whatif (see §`src/eval_materialize.rs` below)
- `document.expressions` → `[Seq Expression]` (already a Seq; the evaluator comment notes: "builtin_eval expects [Seq Expression]")

The SCN single-expression-per-record invariant (every `to-tinct` output is a single-line bracket expression) means each `[expand [load s]]` call produces exactly one document containing exactly one expression. `flat-map` is therefore the wrong combinator for the stream path — its variable-arity generality never fires, and its eager `reduce`-based implementation would force the entire stdin Seq before yielding a single output element, defeating streaming.

The correct combinator is `map` with a `parse-expr` helper that extracts the single expression directly:

```tinct
# parse-expr — extract the single Expression from a single-record SCN string.
# Relies on the SCN invariant: one balanced expression per line → one document,
# one expression. Not valid for multi-document or multi-expression programs.
parse-expr: [fn [let s]
  [head [head [expand [load s]].documents].expressions]]
```

The stream input formatter then becomes:

```tinct
[map stream.parse-expr
  [stream.balanced-exprs [lines %stdin]]]
```

`map` over a lazy Seq is lazy: each element is thunked, the head forces only the first record, and the tail is deferred until the consumer demands it. This is true O(1)-memory streaming.

`program-exprs` is still provided in the codec for callers that load multi-document files and need all expressions:

```tinct
program-exprs: [fn [let prog]
  [flat-map [fn [let doc] doc.expressions] prog.documents]]
```

But it is not used on the stream critical path. No new Rust builtin required. `load` + `expand` + `parse-expr` is the full parse-and-expand pipeline for SCN records.

The codec provides two functions:

**`bracket-count`** — net open bracket depth in a string, accounting for string literals and comments:

```tinct
bracket-count: [fn@Int [let s@String]
  [reduce
    [fn [let st ch]
      [if st.done  st
        [if st.escape  [merge st [escape: false]]
          [if st.in-string
            [if [= ch "\\"]  [merge st [escape: true]]
            [if [= ch "\""]  [merge st [in-string: false]]
                             st]]
            [if [= ch "#"]   [merge st [done: true]]
            [if [= ch "["]   [merge st [depth: [+ st.depth 1]]]
            [if [= ch "]"]   [merge st [depth: [- st.depth 1]]]
            [if [= ch "\""]  [merge st [in-string: true]]
                             st]]]]]]]]
    [depth: 0  in-string: false  escape: false  done: false]
    [str-chars s]].depth]
```

**`balanced-exprs`** — groups a Seq of strings into complete balanced tinct expressions, skipping blank lines, comment lines, and `---` separators:

```tinct
# Two-dict encapsulation pattern — scan is a private helper, not exported.
[
  scan: [fn [let ls acc depth]
    [if [= [] ls]
      [if [= "" [trim acc]] []
        [cons acc []]]
      [if [or [= "" [trim [head ls]]]
               [starts-with? "#" [trim [head ls]]]
               [= "---" [trim [head ls]]]]
        [scan [tail ls] acc depth]
        [if [and [<= [+ depth [bracket-count [head ls]]] 0]
                 [not [= "" [trim [str acc [head ls] "\n"]]]]]
          [cons [str acc [head ls] "\n"] [scan [tail ls] "" 0]]
          [scan [tail ls] [str acc [head ls] "\n"] [+ depth [bracket-count [head ls]]]]]]]]
]
[
  balanced-exprs: [fn [let lines]
    [scan lines "" 0]]
]
```

**Constraint: SCN records are single-line bracket expressions.** `balanced-exprs` skips lines that are blank, start with `#` (comment), or equal `---` (document separator). This is safe because `to-tinct` always emits records as single-line dict expressions (`[k: v  ...]`) that can never start with `---`. The `---` skip is a parser-convenience for interactive use, not a semantic decision.

**Depth constraint: `balanced-exprs` accumulates at most ~256 consecutive non-blank lines per expression** before hitting `MAX_EVAL_DEPTH`. The `[scan rest acc depth]` accumulating branch (for multi-line expressions) is a direct recursive call — each continuation of a partially-parsed expression adds one frame. For SCN streams from `to-tinct`, this is not a concern: every record is a single line, so `bracket-count` returns depth 0 after each line and the accumulating branch is never taken (only the blank-line skip branch and the `[cons a ...]` emit branch are used). For general multi-line tinct program input piped via `-i stream`, expressions spanning more than ~256 lines will fail with a depth error. This is an accepted scope constraint: `-i stream` is designed for SCN records, not for parsing arbitrary multi-line tinct programs.

**Constraint: `bracket-count` handles only single-line strings.** The `bracket-count` function tracks bracket depth, in-string state, and comment state character by character. It correctly handles single-quoted strings (`"..."`) including escaped quotes. It does **not** handle triple-quoted strings (`"""..."""`) — a `[` or `]` inside a triple-quoted string would be miscounted. The stream format therefore constrains `to-tinct` to never emit `"""` triple-quoted strings in its output (the SCN table above uses single-line `"..."` quoting exclusively). Any extension that allows triple-quoted strings in SCN output would also require rewriting `bracket-count` with a triple-quote state machine.

The complete `stdlib/codecs/stream.llt` exports three functions:

```tinct
[
  bracket-count:  ...   # as above
  balanced-exprs: ...   # as above
  parse-expr:     ...   # as above
  program-exprs:  ...   # as above — for multi-document use, not the stream critical path
]
```

### Streaming Input Formatter — `stdlib/cli/in/stream.llt`

```tinct
# Stream input formatter
# Reads %stdin as a truly lazy Seq of tinct expressions, one per SCN record.
[
  stream: [include %libdir "codecs/stream.llt"]
  [map stream.parse-expr
    [stream.balanced-exprs [lines %stdin]]]
]
```

`---` separators are skipped by `balanced-exprs`. EOF terminates the Seq. Each balanced expression string is passed to `stream.parse-expr` which calls `[expand [load s]]` (parse + macro expansion) and extracts the single `Expression` node. `map` over a lazy Seq is lazy — only one record is parsed and expanded at a time, as the consumer demands elements. The result is a `[Seq Expression]` — the pipeline input (`%`) to the user program.

### Streaming Output: `emit`, `%emit`, and the Output Program Contract

`eval-programs` creates a `%emit` channel and threads it through the full program list, calling `eval-programs` with all programs in sequence. `%` is threaded sequentially — the return value of each program becomes `%` for the next. The CLI provides `%stdout` and other capability handles but does not touch `%emit`.

`emit v` in user code sends `v` to `%emit`. Call sites are agnostic to the serializer — the output program (last in the list) decides how to write each received value to `%stdout`.

**Bounded channels and concurrent draining.** The `%emit` channel is bounded (64 slots). If a user program emits more values than the channel can buffer before the output program starts draining, `[send %emit v]` will block. Since `%` is lazy, the output program forces it (driving user-prog's lazy computation) while concurrently draining `%emit` via `drain: [task ...]`. This works when the output program runs in the same tokio `LocalSet` — cooperative scheduling interleaves the force and the drain. For programs that emit large volumes, the CLI may need to spawn user programs and the output formatter as concurrent tasks (see §`src/main.rs`).

#### Output Program Contract

Every output program receives:

- **`%`** — the lazy return value of the previous pipeline stage. Forcing this kicks off the lazy evaluation cascade, driving any `map`/`filter`/`each` computation. The output program is responsible for this forcing — without it, a program that returns a filtered Seq never evaluates.
- **`%emit`** — the emit channel, shared with all upstream programs in the pipeline. Values arrive here as user programs call `emit`.

These must be handled **concurrently within the output program**: forcing `%` drives the program's evaluation and will trigger any `emit` calls in the pipeline, so the channel drain must run simultaneously or deadlock.

The three responsibilities:

1. **Drain `%emit`** — receive emitted values as they arrive; serialize each to stdout.
2. **Force `%`** — materialize the lazy return value to drive the evaluation cascade; serialize every Seq element or scalar return value, including null (`[]`).
3. **Await both** before exiting.

#### `stdlib/cli/out/stream.llt`

```tinct
# Sequential document expressions — each is forced in order.
# 1. Drain the emit channel concurrently.
[drain: [task
  [loop-select [context]
    [seq [ch: %emit  handler: [fn [let v]
      [write-handle %stdout [to-tinct v]]]] []]
    identity]]]

# 2. Force the return value to drive the lazy evaluation cascade.
#    Forcing % drives program evaluation, triggering any emit calls.
[if [seq? %]
  [reduce [fn [let _ x]
    [write-handle %stdout [to-tinct x]]] [] %]
  [write-handle %stdout [to-tinct %]]]

# 3. Wait for drain to finish consuming any emits triggered during forcing.
[await drain]
```

A user writing a custom output formatter follows the same contract: start a `drain` task on `%emit`, force `%`, await the task. Only the serializer changes.

**Error handling in the drain task.** `loop-select` returns `[]` when all channels are exhausted — no exception for normal termination. This means no `try` is needed in the drain task. Serialization errors from `[SERIALIZER v]` (e.g., `to-tinct` encountering a non-serializable `Handle`) propagate naturally up through the task and are visible to the caller of `[await drain]`.

#### Programs emit records lazily

```tinct
# filter.llt — emit matching spans; forcing % drives the each+emit cascade
[each [fn [let s]
  [if [> s.stall-us 0]
    [emit s]
    []]] %]
```

```sh
tinct run -i stream -o stream filter.llt < spans.llt-stream \
  | tinct run -i stream analyze.llt
```

### Pipeline Composition

```sh
# Lazy tinct → tinct pipeline
tinct run -i stream -o stream filter.llt < spans.llt-stream \
  | tinct run -i stream analyze.llt

# Stream → jq: use -o json for NDJSON-compatible output
tinct run -i stream -o json filter.llt < spans.llt-stream | jq .stall-us

# Custom output formatter: user writes their own
tinct run -i stream -o my-formatter filter.llt < spans.llt-stream

# Profiling: Rust writer → tinct reader
tinct run --profile spans.llt-stream program.llt
tinct run -i stream scripts/profile/materialize.llt < spans.llt-stream
```

Each `-o stream | -i stream` stage receives stdlib-closed expressions, evaluates them, and returns a new Seq for the next formatter to process. Structure flows through the pipeline rather than being collapsed at each boundary.

### Rust-Side Serializer

The canonical tinct formatting functions live in `src/lexer.rs` (token-level) and `src/surface_fmt.rs` (expression-level). `Value::to_tinct` dispatches to these; all Rust code that produces tinct text goes through `Value::to_tinct`.

```rust
// src/lexer.rs — token-level formatters, co-located with the corresponding parsers.
// These define the canonical tinct literal syntax in both directions.
pub fn fmt_int(n: i64) -> String { ... }
pub fn fmt_float(f: f64) -> Result<String, String> { ... }  // errors on NaN/Inf
pub fn fmt_bool(b: bool) -> &'static str { ... }
pub fn fmt_string(s: &str) -> String { ... }
pub fn fmt_decimal(d: &Decimal) -> String { ... }
pub fn fmt_bigint(n: &BigInt) -> String { ... }
pub fn fmt_bytes(b: &[u8]) -> String { ... }

// src/surface_fmt.rs — expression-level formatters, co-located with the grammar.
// Intended to converge with the parser as the parse/unparse pair for each form.
pub fn fmt_dict(map: &IndexMap<Key, ThunkId>, ctx: Option<&Arc<EvalContext>>) -> Result<String, String> { ... }
pub fn fmt_seq(head: ThunkId, tail: ThunkId, ctx: Option<&Arc<EvalContext>>) -> Result<String, String> { ... }
pub fn fmt_variant(tag: &str, payload: Option<ThunkId>, ctx: Option<&Arc<EvalContext>>) -> Result<String, String> { ... }
pub fn fmt_fn(params: &[Param], body: &Arc<Spanned<CoreExpr>>, env: &Environment, ctx: &Arc<EvalContext>) -> Result<String, String> { ... }
pub fn fmt_expression(node: &Arc<SurfaceNode>) -> String { ... }

// src/value.rs — inherent method, dispatches to the above.
// ctx is Option: None is valid when the caller guarantees no Function values are present
// (e.g. profiling path serializing scalar dicts). Function serialization requires Some(ctx)
// for stdlib membership checks.
impl Value {
    pub fn to_tinct(&self, ctx: Option<&Arc<EvalContext>>) -> Result<String, String> {
        match self {
            Value::Int(n)    => Ok(fmt_int(*n)),
            Value::Float(f)  => fmt_float(*f),
            Value::Dict(map) => fmt_dict(map, ctx),
            // ... all serializable cases
            Value::Function { .. } => match ctx {
                Some(ctx) => fmt_fn(..., ctx),
                None      => Err("Function serialization requires EvalContext".into()),
            },
            _                => Err(format!("no tinct representation for {}", self.type_name())),
        }
    }
}
```

The profiling flush thread constructs a `Value::Dict` from each `SpanRecord` and calls `val.to_tinct(None)`. Same code path as the `to-tinct` builtin — no special-casing, no drift possible.

## What Would Change

### `stdlib/loader.llt` (update)

**Current** (after builtin-privacy): `eval-program` and `eval-programs` thread `%` through programs; no `%emit` handling.

**Proposed:** `eval-programs` creates the `%emit` channel and `eval-program` handles all document headers: `--- uses:` module injection, named outputs (`--- %name` binds result as `%name` for subsequent documents via the `$k` dynamic key syntax), and `%emit` injection. The CLI does not touch `%emit`.

```tinct
--- uses: ["core"]
[
  # eval-program gains emit-ch as a third parameter and handles all --- headers:
  # - --- uses: ["core"]  → module bindings injected via scope:
  # - --- %name           → result bound as %name for subsequent documents
  # Named outputs use $k dynamic key syntax: [$k: val] where k is a string variable.
  eval-program: [fn [let prog initial-input emit-ch]
    [state: [builtin-reduce
      [fn [let state doc]
        [val: [builtin-eval doc.expressions
                 expects: doc.expects
                 scope:   [builtin-merge
                             [builtin-merge [%emit: emit-ch] state.named]
                             [builtin-reduce builtin-merge []
                               [builtin-map builtin-module doc.uses]]]
                 program: prog
                 %: state.percent]
         k:   [str "%" doc.name]]
        [builtin-merge state
          [percent: val
           named:   [if [= [] doc.name]
                        state.named
                        [builtin-merge state.named [$k: val]]]]]]
      [percent: initial-input  named: []]
      prog.documents]]
    state.percent]

  # eval-programs creates the %emit channel once, shared across all programs.
  # The first dict body establishes emit-ch as a local binding; the second
  # (the reduce) is what eval-programs returns. emit-ch is captured by the
  # inner reduce callback. The last program (output formatter) drains the
  # channel via drain: [task ...].
  eval-programs: [fn [let programs initial-input]
    [emit-ch: [builtin-channel 64]]
    [builtin-reduce
      [fn [let percent prog]
        [eval-program prog percent emit-ch]]
      initial-input
      programs]]
]
```

`%emit` is no longer a CLI-injected cap — it is created and owned by `eval-programs`. The CLI only injects true capability handles: `%libdir`, `%cwd`, `%stdin`, `%stdout`.

**`--- expects:` validation.** `doc.expects` is a new field on `Value::Document` (implemented in `src/eval_materialize.rs`). It returns `[]` when the document has no `--- expects:` header, or a `Value::Expression` wrapping a TypeAssert node (`[@expects-annotation %]`) when one is present. The TypeAssert node is constructed from `SurfaceDocument.expects: Option<Spanned<Annotation>>` at field-access time.

`builtin-eval` gains an `expects:` named argument. When `expects:` is non-null, `builtin-eval` evaluates the TypeAssert expression with `%:` bound to the incoming percent value before evaluating `doc.expressions`. The TypeAssert raises a runtime type error if `%` does not satisfy the annotation; otherwise it returns the (possibly Guarded-wrapped) percent value and evaluation proceeds. The resolved type is looked up from `program.types` (the `TypeAnnotationTable` carried in `Value::Program`) using the expression's span — no re-derivation from the annotation syntax is needed.

This is a one-line change in `eval-program` (`expects: doc.expects` added to the `builtin-eval` call) and two Rust changes: the `"expects"` field arm in `eval_materialize.rs` and the `expects:` named-arg handling in `builtin_eval` in `src/builtins_meta.rs`.

**Prelude:** `stdlib/prelude.llt` re-exports `eval-program` and `eval-programs` (added by builtin-privacy T-735). These must be updated to the new signatures shown above, substituting `module` for `builtin-module` per prelude convention.

### `stdlib/codecs/stream.llt` (new)

**Current:** Nothing.

**Proposed:** `bracket-count`, `balanced-exprs`, and `program-exprs` as defined above. Pure tinct — no new Rust builtins. All three functions are independently useful for REPL-like tools, syntax highlighters, and custom stream parsers.

```tinct
# program-exprs — extract [Seq Expression] from a Value::Program.
# Uses existing field access: program.documents → [Seq Document],
# document.expressions → [Seq Expression].
program-exprs: [fn@[return: Unknown  doc: """
Extract all expression nodes from a parsed program as a flat Seq of Expression values.

Iterates over all documents in the program and collects their expression items,
flattening across document boundaries into a single lazy Seq.
Used by the stream input formatter to convert parsed stream records to eval-ready nodes.
"""] [let prog]
  [flat-map [fn [let doc] doc.expressions] prog.documents]]
```

The complete `stdlib/codecs/stream.llt` exports all three:

```tinct
[
  bracket-count:  ...   # as above
  balanced-exprs: ...   # as above
  parse-expr:     ...   # as above — SCN critical path (map, lazy)
  program-exprs:  ...   # as above — multi-document use (flat-map, eager)
]
```

**Impact:** New file, ~65 lines.

### `stdlib/cli/in/stream.llt` (new)

**Current:** Nothing.

**Proposed:** `[map stream.parse-expr [stream.balanced-exprs [lines %stdin]]]` — pure tinct, no new builtins. `parse-expr` in `stdlib/codecs/stream.llt` calls `[expand [load s]]` and extracts the single `Expression` from the resulting `Value::Program`. `map` over a lazy Seq is lazy — records are parsed one at a time as the consumer demands them, giving true O(1)-memory streaming. Activated by `-i stream`.

**Impact:** New file, ~4 lines.

### `stdlib/cli/in/json.llt` (rewrite)

**Current:** `[json.from-json %stdin]` — reads all of stdin and parses it as a single JSON value.

**Proposed:** Rewritten to a streaming JSON reader using `balanced-objects` in `stdlib/codecs/json.llt`. Reads `%stdin` as a lazy Seq of balanced JSON objects — each read yields the next complete JSON value by counting `{`/`}` and `[`/`]` brackets (accounting for string literals), yielding when depth returns to 0. This is the JSON analogue of `balanced-exprs` in the stream codec. Produces `[Seq Value]` — each element is a parsed tinct value. Handles both NDJSON (one object per line) and pretty-printed multi-document JSON streams (objects spanning multiple lines) naturally.

`stdlib/codecs/json.llt` gains a `balanced-objects` function: reads lines from a handle, accumulates until JSON bracket depth returns to 0, yields the accumulated string, passes it to `from-json`. Uses the same state-machine approach as `balanced-exprs` but with `{`/`}` and `[`/`]` both counting (JSON has no `#` comments; string handling identical to tinct: `"..."` with `\` escapes).

**Impact:** Rewrite of `stdlib/cli/in/json.llt` (~4 lines). New `balanced-objects` function added to `stdlib/codecs/json.llt` (~25 lines).

### `stdlib/cli/in/ndjson.llt` (deleted)

**Current:** Reads `%stdin` line-by-line, parses each non-empty line as JSON, produces `[Seq Value]`.

**Proposed:** Deleted. `-i json` with the `balanced-objects` reader handles NDJSON naturally (each line is a complete JSON object, depth returns to 0 after each line). `-i ndjson` is fully redundant.

### `stdlib/cli/out/` — all formatters rewritten

Every existing output formatter follows the old contract: receive `%`, compute a String, return it; the CLI materializes and prints. All must be rewritten to the new contract: drain `%emit`, force `%`, emit each serialized value, await.

The shared rewrite pattern — substituting only the serializer. Output programs are emit consumers: they receive from `%emit` and write to `%stdout`. They never call `emit`.

**`loop-select` calling convention:** `loop-select` takes three arguments: `context` (a Context value), `sources` (a `[Seq {ch: Channel  handler: Fn}]` — each element is a dict with `ch:` and `handler:` fields), and `handler` (a function applied to each received value, usually `identity`). When all channels are exhausted (all senders dropped), `loop-select` returns `[Closed]`.

**`select-once` returns a nominal result, not an error.** The underlying `select-once` primitive (B-192) returns `[Ok v]` when a value arrives on any channel, or `[Closed]` when all channels are closed. `[Closed]` is a nominally-typed unit constructor that can only ever mean "channel exhausted" — it cannot be confused with any emitted value regardless of what producers emit. `[Ok v]` wraps the actual received value, so a producer emitting `[]` produces `[Ok []]`, which the match correctly handles as a legitimate received value.

`loop-select` is therefore a pure tinct function:

```tinct
loop-select: [fn [let context sources handler]
  [match [select-once context sources]
    [Closed]: [Closed]
    [Ok v]:   [[handler v] [loop-select context sources handler]]]]
```

`[Closed]` must be declared as a nominal variant in prelude: `[type [Closed]]`. The `broadcast-channel` primitive also introduces `[Lagged n]` (a variant indicating missed messages due to a slow subscriber). Both are declared in prelude: `[type [Closed] [Lagged count]]`. The per-channel handler is only called with actual received values. Channel close surfaces as `[Closed]` in the match and never reaches the handler.

`loop-select` is exported from prelude (its implementation moves from `stdlib/async.llt` into prelude when async.llt is merged).

```tinct
# Output formatters are sequential document expressions — NOT a single dict.
# Dict entries are lazy thunks; only sequential expressions are forced in order.
# loop-select returns [Closed] when channels exhaust — no try needed for normal termination.
[drain: [task
  [loop-select [context]
    [seq [ch: %emit  handler: [fn [let v]
      [write-handle %stdout [SERIALIZER v]]]] []]
    identity]]]

[if [seq? %]
  [reduce [fn [let _ x]
    [write-handle %stdout [SERIALIZER x]]] [] %]
  [write-handle %stdout [SERIALIZER %]]]

[await drain]
```

**`stdlib/cli/out/stream.llt` (new)** — `SERIALIZER = to-tinct`. One new file, ~15 lines.

**`stdlib/cli/out/json.llt` (rewrite)** — `SERIALIZER = to-json`. Currently `[call $builtin-to-json %]`, returning a String. Rewritten to the concurrent contract. Produces one compact JSON value per emit record and per Seq element from the return value, each followed by a newline — compatible with NDJSON consumers and `jq` streaming.

**`stdlib/cli/out/json-pretty.llt` (rewrite)** — `SERIALIZER = to-json-pretty`. Writes one full pretty-printed JSON object per emit record and per Seq element, each separated by a blank line. Not NDJSON — each record spans multiple lines. Intended for human-readable output, not machine pipelines.

**`stdlib/cli/out/raw.llt` (rewrite)** — currently: if String return → write it; if Seq → `[join "\n" %]`; else error. New: `SERIALIZER = [fn [let v] [if [str? v] v [str v]]]` — emit each received value as its string representation. The explicit Seq-error is removed; Seq elements arrive naturally through the drain loop.

**`stdlib/cli/out/llt.llt` (rewrite)** — `SERIALIZER = llt-repr`. Currently `[call $llt-repr %]`. Rewritten to emit `[llt-repr v]` for each record.

**`stdlib/cli/out/yaml.llt` (rewrite)** — currently inline formatter returning `[yaml %]` string. Rewritten: inline `yaml` function definition remains; `SERIALIZER = yaml`. Each emitted/returned value is formatted as YAML and written.

**`stdlib/cli/out/csv.llt` (rewrite)** — currently `[csv %]`. CSV has no natural per-record form since the header depends on knowing the column names. The streaming rewrite handles this as follows:

1. Pull the **first record** explicitly from `%emit` via `[recv %emit]` (wrapped in `try` to handle an empty channel). If the channel is already closed (no emitted values), fall through to `%`.
2. Extract **column headers** from the first record's keys using `[keys first]`. This establishes the column order for the entire output.
3. Write the **header line**: `key1,key2,...\n`
4. Write the **first data row** immediately.
5. **Drain remaining `%emit` records** via `loop-select`, writing each as a data row using the captured column order (extra keys ignored, missing keys produce empty cells).
6. **Force `%`** — if it is a Seq, write each element (including null) as a data row; if it is a scalar dict, write it as a data row.
7. **Await** the drain task.

Column order is fixed at step 2 and never changes. Records that don't match the column set (wrong keys) write empty cells for missing columns rather than erroring, so heterogeneous streams produce valid (if sparse) CSV. If no records arrive from either `%emit` or `%`, no output is written (no header either).

**`stdlib/cli/out/toml.llt` (rewrite)** — currently `[toml %]`. Rewritten to `SERIALIZER = toml`.

**`stdlib/cli/out/env.llt` (rewrite)** — currently `[env %]`. Rewritten to `SERIALIZER = env`.

**`stdlib/cli/out/none.llt` (rewrite)** — currently returns `""` and is only invoked by explicit `-o none`. New role: **the default output program** when no `-o` flag is given. Drains `%emit` discarding all values, forces `%` discarding all elements (driving the lazy evaluation cascade for side effects), writes nothing to `%stdout`.

```tinct
# stdlib/cli/out/none.llt — sequential document expressions.
[drain: [task
  [loop-select [context]
    [seq [ch: %emit  handler: [fn [let v] []]] []]
    identity]]]

# Force % to drive the evaluation cascade (for side effects).
[if [seq? %]
  [reduce [fn [let _ x] []] [] %]
  []]

[await drain]
```

**Impact:** All ten formatters rewritten. Old "return a String" contract deleted entirely.

### `to-tinct` in prelude

**Current:** No `to-tinct` function.

**Proposed:** `to-tinct: builtin-to-tinct` — prelude wrapper over `Value::to_tinct(ctx)` in `src/value.rs`. Returns the SCN of any value as a String. No mode magic — it is a plain function callable anywhere.

**Impact:** New builtin registration + prelude entry.

### `src/eval_materialize.rs` — `program.documents` field access

**Current:** `program.documents` returns `Value::Dict` with integer keys (`Key::Int(0)`, `Key::Int(1)`, …).

**Proposed:** Return `Value::Seq` — a linked-list cons chain of `Value::Document` nodes, using the same construction pattern already used by `document.expressions` (lines 2508–2533). The Rust-side `prog.documents` struct field (`Vec<Spanned<Document>>`) is unchanged — only the tinct value returned by field access changes.

```rust
// src/eval_materialize.rs — replace the Dict-building loop for "documents":
"documents" => {
    let docs = &prog.documents;
    if docs.is_empty() {
        Value::Dict(indexmap::IndexMap::new()) // empty [] sentinel
    } else {
        // Build Seq tail from last element back to index 1 (same pattern as document.expressions).
        let mut tail_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(indexmap::IndexMap::new()), // end sentinel
            access_span.clone(),
        )));
        for doc_spanned in docs.iter().rev().take(docs.len() - 1) {
            let head_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Document(Arc::new(doc_spanned.node.clone())),
                access_span.clone(),
            )));
            tail_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Seq { head: head_id, tail: tail_id },
                access_span.clone(),
            )));
        }
        // First document is the outermost head.
        let head_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Document(Arc::new(docs[0].node.clone())),
            access_span.clone(),
        )));
        Value::Seq { head: head_id, tail: tail_id }
    }
}
```

**Impact:** Minimal — one match arm in `eval_materialize.rs`. No Rust callers use `program.documents` as a tinct Value; all Rust-side accesses use the underlying `Vec` directly via struct field access.

### `src/surface_fmt.rs` (new)

**Current:** Nothing.

**Proposed:** Expression-level tinct formatters, co-located with the grammar so the parse/unparse pair for each tinct expression form is visible together. Intended to converge with `src/parser.rs` over time as the canonical parse↔format definition for each form.

- `fmt_dict`, `fmt_seq`, `fmt_variant`, `fmt_fn`, `fmt_expression` — as described in §Rust-Side Serializer above.
- `fmt_fn` implements the closure SCN algorithm (steps 1–4): free-variable identification, env substitution, capture-avoiding gensym rename, serialization.

Token-level formatters (`fmt_int`, `fmt_float`, `fmt_string`, etc.) live in `src/lexer.rs` alongside the corresponding parse logic. This is the canonical location for the tinct literal format in both directions.

**Impact:** New file, ~120 lines.

### `src/value.rs` — `Value::to_tinct` method

**Current:** No `to_tinct` method.

**Proposed:** Add `pub fn to_tinct(&self, ctx: Option<&Arc<EvalContext>>) -> Result<String, String>` as an inherent method on `Value`. Dispatches to `src/lexer.rs` formatters for token-level types and `src/surface_fmt.rs` formatters for expression-level types. The catch-all arm `_ => Err(format!("no tinct representation for {}", self.type_name()))` handles all values for which no stdlib-closed tinct expression exists — no explicit list of non-serializable types required.

**Impact:** ~30 lines added to `src/value.rs`.

### `src/stream.rs` (new)

**Current:** Nothing.

**Proposed:** The `builtin-to-tinct` builtin registration, which calls `val.to_tinct(ctx)`.

**Impact:** New file, ~20 lines.

### Additional channel primitives (`src/builtins_async.rs`)

Three new channel primitives needed for streaming pipelines. All are backed by Tokio; higher-level patterns (pub/sub routing, fan-out trees) are built in tinct on top of these.

**`broadcast-channel N → BroadcastChannel`** — backed by `tokio::sync::broadcast::channel(N)`. Returns a single `BroadcastChannel` value from which subscribers and publishers are derived. Multiple subscribers can each call `recv` on their own subscriber-channel. When a value is sent on the publish side, all subscribers receive it. The backing ring buffer holds at most N messages; slow subscribers receive `[Lagged n]` (a variant indicating they missed `n` messages) instead of a value. When all publishers drop, subscribers receive `[Closed]`. Multiple senders are supported by sharing the publish side with multiple tasks. In tinct, fan-out is straightforward:

```tinct
[
  chans:      [broadcast-channel 64]
  subscriber: chans.0
  publisher:  chans.1
]
```

Pub/sub topic routing, filtering, and subscription management are implemented in tinct on top of `broadcast-channel`.

**`oneshot-channel → [Seq Channel Channel]`** — backed by `tokio::sync::oneshot`. Returns `[receiver-channel sender-channel]`. Exactly one value is sent on `sender-channel`; the single `recv` on `receiver-channel` returns it. Subsequent sends or receives return `[Closed]`. Used for request/response patterns where a task wants to await a single reply:

```tinct
[
  reply-chans: [oneshot-channel]
  reply-recv:  reply-chans.0
  reply-send:  reply-chans.1
]
[task [process-request request reply-send]]
[recv reply-recv]  # blocks until the reply arrives
```

**`try-send channel value → [Ok] | [Full]`** — non-blocking send backed by `mpsc::try_send`. Returns `[Ok]` if the value was sent, `[Full]` if the channel was full (value dropped). Never suspends. Used for drop-newest lossy channels where producers must not stall:

```tinct
# Telemetry that drops metrics when the consumer is slow
[match [try-send metrics-channel datapoint]
  [Ok]:   []  # sent successfully
  [Full]: []]  # dropped — that's fine
```

`try-send` on a full `broadcast-channel` always succeeds (oldest message dropped) — `try-send` is only meaningful for mpsc channels where "full = drop newest" is the desired behaviour.

**Prelude exports:** `broadcast-channel`, `oneshot-channel`, `try-send` — all added to prelude's public dict after S-786 lands (requires `--- uses: ["core"]` injection).

**`select-once` redesign — `[Ok v]` / `[Closed]` return protocol.** `[Closed]` is introduced by this whatif (declared in prelude alongside `[Lagged n]`). The existing `builtin_select_once` must be updated to match the new protocol that `loop-select` is built on:

- **Current:** returns the raw received value on success; raises `EvalError::user_error("select-once: all channels are closed")` when all channels are exhausted.
- **New:** returns `Value::Variant { tag: "Ok", payload: Some(v) }` on success; returns `Value::Variant { tag: "Closed", payload: None }` when all channels are exhausted (no error). Also gains a `context` first argument for cancellation checking.

This enables `loop-select` to be a pure tinct function that matches on `[Ok v]` / `[Closed]` without any try-catch. The existing `loop-select-impl` in `stdlib/async.llt` (which currently wraps the old error-on-close behavior) must be replaced by the new tinct `loop-select` defined in §Streaming Output above, once it moves into prelude.

**Impact:** ~60 lines in `src/builtins_async.rs`; ~5 lines in `stdlib/prelude.llt`; `stdlib/async.llt` loop-select-impl retired.

### `src/main.rs` — emit channel wiring and deletion of special-case output paths

**Current:** Three separate code paths all do materialization and serialization in Rust:

1. **`run_eval` output path:** When `-o` is present, materializes the formatter's return value, asserts it is a `Value::String`, and prints it directly. The formatter is expected to return a fully-rendered String.

2. **`run_literate_eval` (~line 2941):** Always materializes the return value, calls `visit_value(&val, &eval_ctx, 0, &JsonVisitor, ...)`, then `json_pretty_print`. Comment: *"Always serialize to JSON (emit is purely additive)."*

3. **`run_literate_weave` per-block (~line 3296):** Same `visit_value` + `JsonVisitor` pattern per block. Comment: *"Always serialize the result to JSON (emit is additive)."*

All three embed the assumption that the CLI is responsible for output serialization and that `emit` is a side-effect bolted on top. All three must be deleted.

**Proposed:** The CLI injects `%stdout` (a writable Handle for actual stdout) into the env, then calls `prelude-dict["eval-programs"]` with the full program list. `eval-programs` creates and manages `%emit` internally — the CLI does not touch `%emit`. For programs that emit large volumes, the output formatter handles concurrency internally via `drain: [task ...]`.

**CLI wiring (concrete):**

```rust
// In run_eval — CLI only provides capability handles:
env.write().unwrap().insert("%stdout".to_string(), stdout_thunk);
// Note: %emit is NOT injected here — eval-programs creates it internally.

// Build the program list from CLI args:
// tinct run -i json u1.llt u2.llt -o json
//   → programs = [json-in-prog, u1-prog, u2-prog, json-out-prog]
let eval_programs = prelude_dict.get("eval-programs").expect("prelude exports eval-programs");
let result = invoke_fn(eval_programs, [programs_seq, initial_input], ctx)?;
```

**Channel capacity (64):** The `%emit` channel is bounded at 64 slots. This provides backpressure — if the formatter writes slower than the program emits, `[send %emit v]` suspends. With cooperative scheduling this cannot deadlock when the drain task and the force run on the same `LocalSet`: when the sender suspends on a full channel, Tokio yields to the drain task which consumes a value, making room. An unbounded channel would risk accumulating all emitted values in memory for slow formatters.

**Concurrent tasks:** `Value` is `!Send` (contains `Rc<...>`), so all tasks must use `tokio::task::spawn_local` on the same `LocalSet`. This allows sharing `Arc<ChannelInner>` via the emit channel.

Specifically deleted:

- The `--eval` flag and its handling (`force_eval` branch in `run_eval`) — redundant with the default `none.llt` output program
- The `Value::String` match + `print!` in `run_eval` (and the associated error for non-String formatter return)
- The `materialize` + `visit_value` + `json_pretty_print` block in `run_literate_eval`
- The `visit_value` + `JsonVisitor` block in `run_literate_weave`
- The `JsonVisitor` and `visit_value` imports in `main.rs` (no longer needed in CLI paths)

`run_literate_eval` and `run_literate_weave` gain the same channel-wiring treatment as `run_eval`.

**Impact:** Moderate — replaces the sequential "eval → materialize → print" model with "eval-programs + output formatter handles serialization". The output formatters (`cli/out/*.llt`) become the sole owners of serialization and stdout writing.

**Note on `--eval` and the forcing guarantee.** The `--eval` flag is made redundant by this model — running without `-o` uses `none.llt`, which drains `%emit` and forces `%` (including all Seq elements) without writing anything. `--eval` must be removed from the CLI and all documentation.

**Behavior change acknowledged:** In the current model, a program can `tinct run program.llt` without `-o` and the return value is never forced (lazy, no output). After this change, running without `-o` always forces `%` (via `none.llt`'s `[reduce [fn [let _ x] []] [] %]`). This is intentional: the new model requires `%` to be forced to drive the evaluation cascade for side effects and `emit` calls.

### `src/profiling.rs`

**Current:** Already serde-free. `SpanRecord` uses `#[derive(Debug, Clone)]` only. The background flush thread calls `to_ndjson_line()` directly.

**Proposed:** Replace `to_ndjson_line()` with: construct a `Value::Dict` from each `SpanRecord`, call `val.to_tinct(None)`. The `None` ctx is valid because `SpanRecord` fields are scalars only — no Function values. The NDJSON path is deleted; `--profile` always writes `.llt-stream`. Analysis scripts use `-i stream` exclusively.

**Impact:** Minor — `to_ndjson_line()` and `to_json_object_pretty()` removed. The flush thread and snapshot path both migrate to stream format through the canonical `Value::to_tinct` path.

### `scripts/profile/` and `justfile`

**Current:** Profile targets write `spans.ndjson` and use `-i ndjson`.

**Proposed:** Migrate fully to stream format. NDJSON profiling path deleted.

```sh
tinct run --profile spans.llt-stream program.llt
tinct run -i stream scripts/profile/materialize.llt < spans.llt-stream
```

**Impact:** Minor — justfile targets updated; NDJSON profiling path removed.

### `doc/12-tooling.md`

**Current:** Profiling section references `spans.ndjson` and `-i ndjson`. Output formatter section describes the old String-return contract. Input formatter section describes `-i json` as single-value and `-i ndjson` as a separate mode.

**Proposed:** Rewrite the output formatter contract section to describe the new `%emit`/`%stdout` concurrent model. Update `-i json` documentation to describe streaming balanced-object reading. Remove `-i ndjson` (deleted). Add `-i stream` and `-o stream`. Note that `to-tinct` is available for custom serializers. Also update `doc/08-evaluation.md` (emit semantics) and `doc/09-documents.md` (pipeline model).

**Impact:** Moderate — two sections rewritten, one section added.

## Prerequisites

**builtin-privacy sprints (S-785, S-786).** This whatif depends on the full builtin-privacy implementation:

- `eval-programs` and `eval-program` exported from prelude (T-735, S-786)
- `--- uses: ["core"]` header in prelude and stdlib files (S-786)
- `builtin-module`, `builtin-eval scope:`, `document.uses` field access (S-785)
- Async utilities (`task`, `await`, `send`, `recv`, `loop-select`, etc.) merged into prelude from `stdlib/async.llt` (T-733, S-786)

**Prelude exports.** Output formatters use `task`, `await`, `send`, `recv`, `select-once`, `cancelled?`, `context`, `loop-select`, `write`, `each`, `seq?`, `dict?`, `str-chars`, `lines`, `load`, `expand`, `map`, `flat-map`, `filter`, `starts-with?`, `trim`, `str`, `reduce` — all available from prelude after S-786.

**B-168 done.** `write` was renamed to `builtin-write` and re-exported from prelude. `w: builtin-write` is already in prelude. No further action needed.

**No new Rust builtins for the stream codec.** `program-exprs`, `parse-expr`, `balanced-exprs`, `bracket-count` are pure tinct using existing `program.documents` (already returning `[Seq Document]` after the `program.documents` → Seq change below) and `document.expressions` field access.

**`serde_json` dependency.** `src/profiling.rs` is already serde-free. The `serde_json` dependency remains in the LSP feature only, where it is architecturally required.

**Doc chapters.** In addition to `doc/12-tooling.md`, the `emit` model change, `%emit`/`%stdout` injection, and output program contract require updates to `doc/08-evaluation.md` (§emit semantics) and `doc/09-documents.md` (§Pipeline model).

## References

- Peyton Jones, S. (1987). *The Implementation of Functional Programming Languages.* Prentice Hall. — Lazy I/O: a Seq whose spine is driven by the consumer; the model for the lazy `tail` thunk in the stream reader. Partial evaluation by specialization: the formal model for the SCN closure case.
- Jones, N.D., Gomard, C.K., and Sestoft, P. (1993). *Partial Evaluation and Automatic Program Generation.* Prentice Hall. — Binding-time analysis; the formal basis for the SCN algorithm's treatment of closures and free variable substitution.
- ndjson.org (2014). *Newline Delimited JSON.* — The streaming JSON convention; `-o json` produces one compact JSON value per record (NDJSON-compatible). `-i json` handles both NDJSON and multi-line JSON streams via balanced bracket counting.
