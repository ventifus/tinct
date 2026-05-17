# What If: Macro System v2 — Parse-Stage Delivery and Declarative Patterns for tinct

**State:** Proposal

What would it take to make tinct's macro system powerful enough that user-programmers — not language designers — can implement any syntactic extension, including softening the strong positions the core language takes?

## Current State

Tinct's macro system (`defmacro` + `quote`/`unquote`/`unquote-splice`) operates post-parse: macros receive fully-formed `Expr` AST dicts and return AST dicts. The expansion pass runs before type-checking. What is fully implemented:

- `defmacro` (to be renamed `macro`) — procedural AST macros via `ast_to_dict` / `dict_to_ast`
- `quote` / `unquote` / `unquote-splice` — quasiquoting
- `gensym` — manual hygiene for macro-introduced names
- Provenance tracking — dual-span error reporting (macro call site + expansion site)
- `tmpl`, `do`, `begin` macros in stdlib

Two structural gaps remain.

**Argument destructuring is manual.** Every macro receives an opaque `args` sequence and must manually index into it — `[nth args 0]`, `[nth args 1]`, etc. There is no way to declare the expected shape of arguments or dispatch on argument count and structure. Every macro reimplements the same destructuring boilerplate, and callers get no structural error messages when they violate the macro's expectations.

```tinct
# Today: manual indexing, no structural validation
[macro my-if [args]
  [cond: [nth args 0]
   then: [nth args 1]
   else: [nth args 2]]
  [list 'if cond then else]]
```

**Call-semantics erase structure before the macro runs.** A bracket like `[x@Int y@Float]` is parsed as `Call(Annotated("x", Int), [Annotated("y", Float)])` before any macro runs. The flat element sequence the macro might need to work with — `[x@Int, y@Float]` as a list — is unrecoverable from the Call node. This blocks entire classes of structural transformation.

### What's Missing

1. A way for macros to declare their expected argument structure and have the runtime bind named pattern variables to the matched pieces.
2. A way to receive bracket content before call-semantics are applied — enabling macros to inspect and reshape bracket elements that the parser would otherwise consume.
3. A way to produce multiple top-level definitions from one macro invocation.
4. A way for macro bodies to signal structured compile-time errors with source spans.
5. A way for annotated expressions (`n@Int`) to serve as distinct dict keys within macro-defined forms, bypassing the parser's bare-name duplicate detection.

## Why This Matters for tinct

**The language can take strong positions without losing extensibility.** Tinct takes strong syntactic positions — `[let ...]` required in all binding contexts, `[case ...]` required for match arms — to eliminate ambiguity and special cases in the core. These positions are correct. But without a sufficiently powerful macro system, they become immutable walls. A user who prefers writing `[fn [x y] body]` has no recourse. With parse-stage delivery, a user can write a macro that implements the softening. The language's correctness and the user's ergonomic preferences are no longer in conflict.

**Structural macros become writable in tinct.** `derive`-style macros — one invocation produces multiple `instance` declarations — require multi-form output. Macros that generate nested `[match ...]`/`[case ...]` forms, macros that introduce hygienically-named intermediate bindings, macros that validate argument structure and emit precise compile errors — none are expressible today. All fall out directly from this proposal.

**Macro errors become precise.** Today, a macro that receives wrong-shaped arguments either crashes at runtime with a confusing index error or produces malformed AST that fails at type-check with no connection to the macro call. `macro-error` and `span-of` let macros produce errors that point at the exact source location of the problem.

## Design

### The Architecture

The key invariant is the three-layer pipeline:

```
source → [parse: syntactic only] → [transformation pass: user macros] → [type-check: semantic enforcement] → eval
```

**The parser** handles syntax: bracket nesting, token classification, form recognition. It does not enforce semantic rules — that a `fn` parameter list must be `Expr::Let`, that `[let ...]` must appear in binding positions. The parser accepts `[fn anything body]` and produces `FnExpr { params: <whatever>, body: ... }`. It never hard-errors on semantic mismatches.

**The transformation pass** runs next. User macros see the parser's output and reshape it. A `macro fn [let params:flat-list body]` macro intercepts `FnExpr` nodes whose params are not `Expr::Let` and wraps them. The type checker then sees conforming code.

**The type checker** enforces semantic rules. `FnExpr` with non-`Let` params → type error. `[let ...]` absent from a binding position → type error. Semantic enforcement belongs here, not in the parser.

This is correct layering independently of macros. Macros benefit because they occupy the right slot in an already-correct pipeline: the transformation pass runs before semantic enforcement fires.

**Consequence for unified-bindings:** anywhere `doc/whatif/unified-bindings.md` currently states "parse error" for a missing `[let ...]`, this is a type error. The parser StackFrames for `fn`, `class`, `type` accept any first sub-expression; the type checker rejects non-`Let` params. Nothing about the parser architecture changes for macros specifically — it was always wrong to put semantic enforcement in the parser.

---

### AST Types

Macros receive and return values of type `Expr` — a nominal variant type defined in `stdlib/ast.llt` and produced by `ast_to_dict`. This is the same `Expr` tinct's own evaluator works with, exposed as a first-class tinct type. Dispatch on AST node kind is structural pattern matching on the `Expr` variant — the same `[match ...]`/`[case ...]` syntax used everywhere else in tinct.

```tinct
# stdlib/ast.llt

[type Annotation
  [Simple   name: Str]           # @Int, @Bool
  [PropDict entries: Seq@Entry]  # @[return: T  constraint: ...]
  Null]                          # no annotation

[type ReceiveMode  Expr  FlatList]

[type Expr
  [Let        bindings: Seq@Expr]
  [Case       pattern: Expr  body: Expr]
  [VarRef     name: Str]
  [Call       func: Expr  args: Seq@Expr]
  [Annotated  expr: Expr  ann: Annotation]
  [Literal    value: Any]                     # Int, Float, Str, Bool, Null scalars
  [Dict       entries: Seq@Entry]             # keyed dict
  [Seq        elements: Seq@Expr]             # positional sequence
  [Fn         ann: Annotation  params: Expr  body: Expr]
  [Quote      expr: Expr]
  [Unquote    expr: Expr]
  [UnquoteSplice expr: Seq@Expr]
  [Splice     forms: Seq@Expr]
  [Macro      name: Str  params: Seq@MacroParam  body: Expr]
  [Placeholder]]
```

Variant names match their keywords: `Fn` for `fn`, `Let` for `let`, `Case` for `case`, `Macro` for `macro`. No `Decl` suffix needed — there is only one `Let` and one `Macro` in the type.

`gensym` returns `VarRef(name: "prefix__N")` — a genuine `Expr` variant, not a string. `[unquote (gensym "x")]` in a quasiquote splices a `VarRef` node directly, in both binding and reference positions.

Macro bodies that annotate their parameters benefit from full type checking:

```tinct
[macro my-if [let cond@Expr  then@Expr  else@Expr]
  [quote [if [unquote cond] [unquote then] [unquote else]]]]
```

Unannotated macro parameters are `Unknown` — gradual typing means they still compile; you just opt out of structural checking.

---

### `macro` — The Unified Macro Form

`macro` is the single keyword for all AST macros. It accepts `[let ...]` patterns in the argument position — the same syntax as function parameters. Pattern variables bind to the corresponding argument positions.

```tinct
# Before: manual indexing
[macro my-if [args]
  [cond: [nth args 0]
   then: [nth args 1]
   else: [nth args 2]]
  [quote [if [unquote cond] [unquote then] [unquote else]]]]

# With [let ...] pattern
[macro my-if [let cond then else]
  [quote [if [unquote cond] [unquote then] [unquote else]]]]
```

Typed patterns constrain expected shapes:

```tinct
[macro my-assert [let condition@Expr  message@Str]
  [quote [if [unquote condition] true [error [unquote message]]]]]
```

Variadic via `...rest` — already defined in `[let ...]` for function params:

```tinct
[macro my-list [let ...items]
  [quote [list [unquote-splice items]]]]
```

**Multi-arm dispatch.** When a macro needs to handle different argument shapes, `[case ...]` arms dispatch on argument count and structure — the same match syntax:

```tinct
[macro my-and
  [[case [let a]]          a]
  [[case [let a b]]        [quote [if [unquote a] [unquote b] false]]]
  [[case [let a ...rest]]  [quote [if [unquote a] [my-and [unquote-splice rest]] false]]]]
```

**Hygiene.** With `[let ...]` patterns, the template/user-code distinction is structural:
- Names bound in the `[let ...]` argument pattern are *user-code bindings* — they hold pieces of the caller's input AST. No rename needed; they are the user's own names.
- Names introduced by `gensym` in the body are *macro-introduced* — they must not capture caller-scope variables.

```tinct
[macro with-retry [let max-attempts body]
  [counter: [gensym "counter"]]
  # gensym returns a var-ref AST node — [unquote counter] splices it as an identifier
  [quote [let [[[unquote counter]: 0]]
    [while [< [unquote counter] [unquote max-attempts]]
      [unquote body]
      [set! [unquote counter] [+ [unquote counter] 1]]]]]]
```

`counter` is gensym'd — macro-introduced. `max-attempts` and `body` are from the pattern — user-provided. `gensym` returns a `var-ref` AST node (`{type: "var-ref" name: "counter__42"}`), so `[unquote counter]` splices it as an identifier wherever it appears — binding position, reference position, anywhere. No special form needed. The distinction is syntactically explicit. Scope set activation (Phase 2) becomes straightforward: pattern-bound names carry the caller's scope; gensym names carry the macro's scope.

---

### Receive Modes — `param:flat-list`

All macro arguments default to `expr` mode: the argument is delivered as the fully-parsed AST node. For cases where the parser's call-semantics interpretation loses structure the macro needs, a parameter can declare a receive mode using `:mode` after the binding name — the same `:` position used for receive mode in the `[let ...]` binding (not a structural test, which is only valid in case arms):

```tinct
[macro name [let arg:receive-mode  ...] body]
```

Both `macro` and `macro` with receive modes are the **same form at the same pipeline stage** — both run in the transformation pass, post-parse and pre-typecheck. Receive modes only affect how specific arguments are re-processed before being handed to the macro body.

**Receive modes:**

| Mode | What the macro receives | When to use |
|------|------------------------|-------------|
| `expr` | Fully parsed expression (default — omit the annotation) | All ordinary cases |
| `flat-list` | Bracket elements as a sequence, before call-semantics | When element structure matters more than call interpretation |

`flat-list` is the key mode. `[x@Int y@Float]` parses as `Call(Annotated("x", Int), [Annotated("y", Float)])` in `expr` mode — the flat element sequence is gone. In `flat-list` mode, the transformation pass extracts the bracket's entries and delivers `[Annotated("x", Int), Annotated("y", Float)]` as a tinct `Seq`, regardless of how the parser interpreted the bracket form.

**The fn let-softening macro:**

```tinct
# stdlib/syntax.llt — available to any program that opts in
[macro fn [let params:flat-list@Seq@Expr  body@Expr]
  [match [first-or params Null]
    [[case [let _ : Let]]  [quote [fn [unquote params] [unquote body]]]]
    [[case [let _]]            [quote [fn [let [unquote-splice params]] [unquote body]]]]]]

[macro class [let tvars:flat-list@Seq@Expr  ...body@Expr]
  [match [first-or tvars Null]
    [[case [let _ : Let]]  [quote [class [unquote tvars] [unquote-splice body]]]]
    [[case [let _]]            [quote [class [let [unquote-splice tvars]] [unquote-splice body]]]]]]

[macro type [let params:flat-list@Seq@Expr  body@Expr]
  [match [first-or params Null]
    [[case [let _ : Let]]  [quote [type [unquote params] [unquote body]]]]
    [[case [let _]]            [quote [type [let [unquote-splice params]] [unquote body]]]]]]
```

Each macro body is pure tinct. No Rust flags. No hardcoded transformation logic. The infrastructure provides delivery; all logic lives in the macro.

A user who loads `stdlib/syntax.llt` can write `[fn [x@Int y@Float] body]`. A user who does not gets a type error from the type checker. The language's position is strict; the macro system makes it extensible. The user — not the language designer — controls which ergonomic forms are available in their codebase.

**How the pass delivers `flat-list`.** For a registered form name, the transformation pass extracts the bracket's entries from whatever AST node the parser produced:
- From a `Call` node: `[func, arg0, arg1, ...]` (func + all args as elements)
- From a `Dict` node: the dict entries
- From an `Expr::Let` node: the bindings (already flat — pass through unchanged)

The resulting `Seq` is delivered to the macro body. The macro inspects and reshapes it, returning a new AST dict. The pass substitutes the original form with the result and continues.

**Pre-scan for registration.** `macro` declarations with `:flat-list` parameters are scanned from the parsed AST before the transformation pass begins its first walk. This gives the pass a complete registry of registered form names before it processes any of them. Stdlib declarations in `stdlib/syntax.llt` are always pre-loaded. Any form with a `macro` declaration automatically gets neutral key handling from the parser — no duplicate-key enforcement — since the macro body handles structural validation.

**Transformation to fixpoint.** The pass runs until no registered form names appear unvisited in the AST. A macro's output is re-visited. Depth limit 100 per site; total node-count cap 100k.

---

---

### `splice` — Multi-Form Output

A macro returns `[splice form1 form2 ...]` to inject multiple forms into the surrounding context:

```tinct
[macro derive [let targets:flat-list  ...body]
  [splice
    ...[map [fn [let target]
              [quote [instance [unquote target] [unquote-splice body]]]]
           targets]]]

# Usage:
@[derive Equal Comparable]
Point: [type [x@Float  y@Float]]
# Expands to: [instance Equal ...]  [instance Comparable ...]  Point: [type ...]
```

At dict top level, each spliced form becomes a separate dict entry. In expression position, `splice` is a type error — multiple expressions where one is expected.

---

### `macro-error` and `span-of` — Compile-Time Error Signaling

Macro bodies signal structured compile-time errors that point at source locations:

```tinct
[macro-error span message]   # terminate transformation with compile error at span
[span-of expr]               # extract source span from an AST node
```

```tinct
[macro pragma [let name:flat-list  value]
  [if [not [= 1 [length name]]]
    [macro-error [span-of name] "pragma name must be a single bare identifier"]
    [if [not [literal? value]]
      [macro-error [span-of value] "pragma value must be a literal"]
      [list 'pragma [first name] value]]]]
```

`macro-error` raises a `CompileError` at the given span before type-checking. It is surfaced to the user with the same formatting as any other compile error.

---

### AST Inspection Primitives

Macro bodies inspect AST nodes using tinct predicates — the equivalent of Racket's syntax classes, expressed as ordinary tinct functions:

```tinct
# Inspection — use tinct's own [match ...]/[case ...] to dispatch on Expr variants:
#   [match node [[case [let _ : VarRef]] ...] [[case [let _ : Let]] ...]]
#   [match node [[case [let [name: n] : VarRef]] ...]]   # extract VarRef's name field
# No predicate functions needed — variant matching IS the predicate.
[span-of expr]           # extract source span from an Expr node (spans are metadata)

# Quasiquote — primary construction mechanism
[quote expr]             # produce the Expr AST node for expr
[unquote val]            # splice val (an Expr) into the enclosing quote
[unquote-splice seq]     # splice a Seq@Expr into the enclosing quote

# Sequence operations on flat-list deliveries (Seq@Expr)
[first xs] [rest xs]     # element access
[first-or xs default]    # first element, or default if empty

# Gensym and error
[gensym prefix]          # returns VarRef(name: "prefix__N") — a fresh Expr identifier
[macro-error span msg]   # terminate transformation with compile error at span

# Stdlib helpers
[wrap-in-let elems]      # produce Let(bindings: elems) AST node
[let-decl-elems decl]    # extract bindings Seq@Expr from a Let node
```

---

### Explicitly Out of Scope

**`tokens` receive mode** — raw token sequences for embedded DSLs. No concrete tinct use case warrants the security exposure of raw token access. Deferred indefinitely.

**Infix operator registration** — requires hooks into the tokenizer, which is Rust-only by design. Tinct's bracket syntax makes infix operators unnecessary.

**Compile-time type access** — macros run before type-checking and do not see inferred types. Interleaving expansion with type inference (Template Haskell's `reify`) would fundamentally complicate the pipeline. Excluded.

**Character-level lexer hooks** — security exclusion. User code never touches the character stream.

---

## Worked Examples

Each example shows the macro definition, representative inputs, their expansions, and the edge cases that exercise boundary conditions.

Macro bodies are ordinary tinct code. AST nodes are `Expr` variants — dispatch on node kind is structural pattern matching with `[case [let _ : VarRef]]`, `[case [let _ : Let]]`, etc. — the same `[match ...]`/`[case ...]` syntax used everywhere in tinct, applied to tinct's own AST type. `[if ...]` is reserved for simple boolean conditions with no else-if chain; everything with two or more structural outcomes uses `[match ...]`.

---

### Simple 1: `unless` — Single `[let ...]` Pattern

```tinct
[macro unless [let cond body]
  [quote [if [unquote cond] [] [unquote body]]]]
```

```tinct
[unless [> x 10] [emit "x is small"]]
# → [if [> x 10] [] [emit "x is small"]]

[unless false [do-work]]
# → [if false [] [do-work]]
```

**Edge case — body is a sequence:** `[let ...]` patterns bind positionally; the macro sees `cond` and `body` as the first and second arguments. A caller passing three arguments gets an arity error from the pattern binding — the pattern `[let cond body]` requires exactly two arguments.

```tinct
[unless [= x 0] [emit "a"] [emit "b"]]
# arity error: unless expects 2 arguments, got 3
```

---

### Simple 2: `my-or` — Multi-Arm Dispatch

```tinct
[macro my-or
  [[case [let]]            [quote false]]
  [[case [let a]]          a]
  [[case [let a b]]        [quote [if [unquote a] [unquote a] [unquote b]]]]
  [[case [let a ...rest]]  [quote [if [unquote a] [unquote a] [my-or [unquote-splice rest]]]]]]
```

The zero-arg arm returns `[quote false]` — the AST for the literal `false`, not a raw boolean. The one-arg arm returns `a` directly — `a` is already an AST dict (the argument's parsed form), so no quoting needed. The two- and many-arg arms build new AST using quasiquote.

```tinct
[my-or]              # → false
[my-or x]            # → x
[my-or x y]          # → [if x x y]
[my-or x y z]        # → [if x x [my-or y z]]
```

**Edge case — fixpoint:** `[my-or x y z]` expands to `[if x x [my-or y z]]`, which contains another `my-or` call. The transformation pass re-visits and expands again:

```
Pass 1: [my-or x y z]  →  [if x x [my-or y z]]
Pass 2: [my-or y z]    →  [if y y z]
Result: [if x x [if y y z]]
```

The pass runs to fixpoint. The depth limit (100) guards against infinite recursion.

---

### Simple 3: `with-tmp` — Gensym Hygiene

```tinct
[macro with-tmp [let expr body]
  [tmp: [gensym "tmp"]]
  # gensym returns a var-ref node — [unquote tmp] splices it as an identifier
  [quote [let [[[unquote tmp]: [unquote expr]]] [unquote body]]]]
```

```tinct
[with-tmp [expensive-computation] [+ tmp 1]]
# → [let [[tmp__42: [expensive-computation]]] [+ tmp__42 1]]
```

`gensym` returns `VarRef(name: "tmp__42")` — a genuine `Expr` variant. `[unquote tmp]` splices it directly as an identifier wherever it appears: in binding position, in reference position, anywhere. No special splicing primitive needed.

**Edge case — user variable named `tmp`:** Without gensym, the macro would introduce `tmp` and shadow the user's own `tmp`. Gensym produces a node whose name contains `__N` (a suffix that cannot appear in user-written tinct identifiers), so the user's `tmp` is unaffected.

```tinct
[let [tmp: 99]
  [with-tmp [compute] [+ tmp result]]]
# → [let [tmp: 99]
#     [let [[tmp__42: [compute]]]
#       [+ tmp result]]]   # tmp refers to 99; tmp__42 is the computed value
```

`tmp` in `[+ tmp result]` resolves to the user's `99`, not the macro's internal binding. Hygiene preserved.

---

### Complex 1: `fn` Let-Softening — Full Edge Case Coverage

`stdlib/syntax.llt` (opt-in):

```tinct
[macro fn [let params:flat-list@Seq@Expr  body@Expr]
  [match [first-or params Null]
    [[case [let _ : Let]]  [quote [fn [unquote params] [unquote body]]]]
    [[case [let _]]        [quote [fn [let [unquote-splice params]] [unquote body]]]]]]
```

`first-or params Null` returns the first element, or `Null` if empty. The `[case [let _ : Let]]` arm matches if the first element is a `Let` variant (already has `[let ...]`). The wildcard catches `VarRef`, `Annotated`, `Null` (empty params), and anything else — all get wrapped.

**Case 1 — already has `[let ...]`:** Idempotent; pass through unchanged.

```tinct
[fn [let x@Int y@Float] [+ x y]]
# params flat-list: [Let(bindings: [Annotated(x, Int) Annotated(y, Float)])]
# first → Let(...) → [case [let _ : Let]] matches → pass through
# → [fn [let x@Int y@Float] [+ x y]]  (unchanged)
```

**Case 2 — bare params, no annotations:**

```tinct
[fn [x y] [+ x y]]
# params flat-list: [VarRef("x")  VarRef("y")]
# first → VarRef("x") → wildcard arm → wrap
# → [fn [let x y] [+ x y]]
```

**Case 3 — annotated params:**

```tinct
[fn [x@Int y@Float] [+ x y]]
# params flat-list: [Annotated(VarRef("x"), Int)  Annotated(VarRef("y"), Float)]
# first → Annotated(...) → wildcard arm → wrap
# → [fn [let x@Int y@Float] [+ x y]]
```

**Case 4 — empty params (`[fn [] body]`):**

```tinct
[fn [] body]
# params flat-list: []  →  first-or [] Null  →  Null → wildcard arm
# → [fn [let] body]
```

**Case 5 — variadic params:**

```tinct
[fn [f ...args] [map f args]]
# params flat-list: [VarRef("f") Spread(VarRef("args"))]
# first is VarRef, not Let → wrap
# → [fn [let f ...args] [map f args]]
```

**Case 6 — nested fn (inner should not be affected by outer macro):** The transformation pass applies the macro to each `fn` form independently. The inner `fn` is also intercepted.

```tinct
[fn [f xs] [map [fn [x] [f x]] xs]]
# Outer fn: [fn [let f xs] [map [fn [x] [f x]] xs]]
# Inner fn: [fn [let x] [f x]]
# Both expanded independently; no interaction.
```

**Without `stdlib/syntax.llt`:** The type checker receives `FnExpr { params: Call(x, [y]) }` and emits: "fn parameter list must be a `[let ...]` binding declaration." The user sees a type error. Loading `stdlib/syntax.llt` makes the form valid.

---

### Complex 2: `derive` — `splice` for Multi-Form Output

```tinct
[macro derive [let targets:flat-list  ...body]
  [splice
    ...[map [fn [let target]
              [quote [instance [unquote target] [unquote-splice body]]]]
           targets]]]
```

```tinct
@[derive Equal Comparable]
Point: [type [x@Float  y@Float]]
```

Expands to three separate dict entries:

```tinct
[instance Equal    [Point]: [=: [fn [let a b] [and [= a.x b.x] [= a.y b.y]]]]]
[instance Comparable [Point]: [<: [fn [let a b] [< a.x b.x]]]]
Point: [type [x@Float  y@Float]]
```

**Edge case — single target:** `@[derive Equal]` produces one `instance` form plus the annotated definition. The `map` produces a one-element splice; the pass injects it correctly.

**Edge case — splice in expression position:**

```tinct
[str [derive Equal Comparable] "suffix"]
# type error: splice not valid in expression position
```

`splice` is only valid at dict top level. The type checker rejects it in any value position.

---

### Complex 3: `cond` — Multi-Arm Dispatch with Recursive Expansion

`cond` accepts a sequence of `[test body]` clause pairs plus an optional `[else body]` fallback. It chains them into nested `[if ...]` expressions.

```tinct
[macro cond
  [[case [let]]
    [quote [error "cond: no matching clause"]]]
  [[case [let clause]]
    # single clause — must be [else body] or [test body]
    [match [get-or [first-or clause {}] "type" null]
      [[case [let _ : VarRef]]  [quote [unquote [second clause]]]]   # [else body] — VarRef("else")
      [[case [let _]]           [quote [if [unquote [first clause]]  # [test body]
                           [unquote [second clause]]
                           [error "cond: no matching clause"]]]]]]
  [[case [let clause ...rest]]
    [quote [if [unquote [first clause]]
      [unquote [second clause]]
      [cond [unquote-splice rest]]]]]]
```

```tinct
[cond
  [[> x 10]  "big"]
  [[> x 5]   "medium"]
  [else       "small"]]
```

Expansion trace:

```
[cond [[> x 10] "big"] [[> x 5] "medium"] [else "small"]]
→ [if [> x 10] "big"     [cond [[> x 5] "medium"] [else "small"]]]
→ [if [> x 10] "big"     [if [> x 5] "medium" [cond [else "small"]]]]
→ [if [> x 10] "big"     [if [> x 5] "medium"  "small"]]
```

**Edge case — empty `cond`:** The zero-arm case returns a guaranteed runtime error rather than silently producing `null`. Matches Racket's behaviour.

**Edge case — `else` detection:** `[else body]` is detected by checking if the first element of the clause is a `var-ref` named `"else"`. This uses `node.type` dispatch — consistent with how all macro bodies inspect AST structure. If `else` is not in scope, the type checker would flag it as an undefined variable; but `else` is imported from stdlib as a constant `true`, making `[else body]` equivalent to `[if true body (no-match)]`.

**Edge case — the fixpoint:** Each expansion step produces exactly one more `[if ...]` and one recursive `[cond ...]` call. The depth limit (100) bounds this. A `cond` with 101 clauses exceeds the limit and produces a macro depth error.

---

### Complex 4: `pragma` — `macro-error` for Structural Validation

```tinct
[macro pragma [let name:flat-list  value]
  [match [length name]
    [[case 0]
      [macro-error [span-of name] "pragma: name required"]]
    [[case 1]
      [match [get-or [first name] "type" null]
        [[case [let _ : VarRef]]
          [match value
            [[case [let _ : Literal]]  [quote [pragma [unquote [first name]] [unquote value]]]]
            [[case [let _]]            [macro-error [span-of value] "pragma value must be a literal"]]]]
        [[case [let _]]
          [macro-error [span-of [first name]] "pragma name must be a bare identifier"]]]]
    [[case [let _]]
      [macro-error [span-of name] "pragma: exactly one name allowed"]]]]
```

Each level of dispatch is a `[match ...]`: length (integer value), node type (string value), literalness (boolean). No `[if [not ...]]` chains — every branch is a case arm with its own arm body.

```tinct
[pragma optimize true]       # → [pragma optimize true]  ✓
[pragma]                     # compile error at pragma span: "pragma: name required"
[pragma "opt" true]          # compile error at "opt": "pragma name must be a bare identifier"
[pragma optimize x]          # compile error at x: "pragma value must be a literal"
[pragma optimize debug true] # compile error at span: "pragma: exactly one name allowed"
```

Each error points at the exact source location of the violation: the macro uses `span-of` to attach the error to the specific offending token, not the entire `[pragma ...]` call. A caller sees a precise message, not a generic macro failure.

---

## What Would Change

### Unified-Bindings (`doc/whatif/unified-bindings.md`)

**Current:** The proposal states "parse error" for `[fn [x y] body]` (missing `[let ...]`).
**Proposed:** Corrected to "type error." The parser accepts `[fn any-expr body]`; the type checker enforces `Expr::Let` in param position. This removes hard enforcement from StackFrames for `fn`, `class`, `type` and places it in `check_fn_expr` in `src/typecheck.rs`.
**Impact:** Minor in scope; architectural correctness improvement independent of macros.

### `src/parser.rs` — Pre-Scan and Neutral Key Handling

**Current:** Duplicate detection uses bare-name identity unconditionally. `fn`/`class`/`type` StackFrames enforce `Expr::Let`.
**Proposed:** (1) A pre-scan pass collects `macro` declarations with `:flat-list` parameters before the main parse begins. (2) Any form with a `macro` declaration in scope gets neutral key handling — the parser does not apply bare-name duplicate detection for that form's argument positions. (3) `fn`/`class`/`type` StackFrames accept any first sub-expression without error (semantic enforcement moved to type checker).
**Impact:** Moderate — pre-scan; neutral-key flag per form; StackFrame semantic checks removed.

### `src/expand.rs` — Parse-Stage Transformation Pass

**Current:** Expansion pass handles `defmacro` post-parse.
**Proposed:** Rename `defmacro` → `macro`. Extend the pass with receive mode support: when a `macro` declaration has parameters with `:flat-list` mode, re-deliver those arguments as flat element sequences before calling the macro body. Add `flat-list` delivery: extract bracket entries from the parsed AST node (Call args, Dict entries, Let bindings). Add `splice` handling: when a macro returns `Expr::Splice(forms)`, inject into parent context. Update argument binding to use `[let ...]` pattern matching.
**Impact:** Moderate — new delivery logic and splice handling; integrates with existing expansion infrastructure.

### `src/ast.rs` — New Variants

```rust
Expr::MacroDecl {
    name: String,
    params: Vec<(String, ReceiveMode)>,   // ReceiveMode::Expr (default) or FlatList
    body: Box<Spanned<Expr>>,
}

Expr::Splice(Vec<Spanned<Expr>>)
```

**Impact:** Minor — three new variants; exhaustive match arms updated mechanically.

### `src/typecheck.rs` — Semantic Enforcement for Binding Positions

**Current:** `fn`/`class`/`type` param checking happens in the parser.
**Proposed:** Move to `check_fn_expr`, `check_class_decl`, `check_type_alias`: if the first sub-expression is not `Expr::Let`, emit type error "parameter list must be a `[let ...]` binding declaration."
**Impact:** Minor — enforcement moved, semantics unchanged.

### `stdlib/prelude.llt` — New AST Primitives

Add: `span-of`, `wrap-in-let`, `let-decl-elems`, `first-or` (sequence helpers).
Add: `macro-error` as a Rust builtin (`ErrorKind::MacroError` with span).
Add: `stdlib/ast.llt` — defines `Expr`, `Annotation`, `ReceiveMode`, `MacroParam`, `Entry` nominal types. Imported automatically in macro-writing contexts. No predicate functions (`literal?`, `var-ref?`, etc.) — variant pattern matching replaces them all.
Update: `gensym` returns `VarRef(name: "prefix__N")` — a genuine `Expr` variant. `[unquote (gensym "x")]` splices a fresh identifier node directly; no special splicing primitive needed.
Update: `ast_to_dict` produces typed `Expr` variant values rather than plain dicts with string `type:` fields. All existing code consuming AST dicts (`formatter/compact.llt`, `formatter/pretty.llt`, `ast-of`, macro bodies) migrates to variant pattern matching.
**Impact:** Minor — additive.

### `stdlib/syntax.llt` (new file)

`macro` declarations for fn/class/type let-softening (using `:flat-list` receive mode), available to any program that opts in via `[include %libdir "syntax.llt"]`. Not loaded by default — the core language remains strict without it.
**Impact:** New file, ~50 lines.

---

## Prerequisites

- **`defmacro`** — fully implemented; this proposal renames it to `macro` and extends it
- **`unified-bindings`** — `Expr::Let` (the `[let ...]` binding form) must exist; the corrected parser enforcement requires the unified-bindings parser update to not hard-error on missing `[let ...]`
- **`ast_to_dict` / `dict_to_ast`** — already implemented (`ast-dict-core`); macro bodies use them

## References

- Tobin-Hochstadt, S. et al. (2011). "Languages as Libraries." *PLDI '11*, pp. 132–141. ACM. — [`#lang` as user-defined language extension via macros; the principle that language positions should be extensible from user code, not only from language designers]
- Flatt, M. (2016). "Binding as sets of scopes." *POPL '16*, pp. 705–717. ACM. — [scope sets for hygienic macro expansion; the formal model for distinguishing macro-introduced bindings from user-code bindings]
- Flatt, M. & PLT (2010). "Reference: Racket." §Syntax Classes (`syntax-parse`). — [declarative pattern declarations for macro arguments as the ergonomic foundation of the macro system; model for `[let ...]` patterns in `macro`]
- Graham, P. (1993). *On Lisp.* Prentice Hall. — [macros as functions from code to code; macro body as ordinary program; the principle that all macro logic should live in the macro, not the infrastructure]
- Kohlbecker, E., Friedman, D.P., Felleisen, M. & Duba, B. (1986). "Hygienic Macro Expansion." *LFP '86*, pp. 151–161. ACM. — [first formal definition of hygiene; gensym as minimal hygiene guarantee; structural impossibility of collision]
- Dybvig, R.K., Hieb, R. & Bruggeman, C. (1993). "Syntactic Abstraction in Scheme." *Lisp and Symbolic Computation*, 5(4), 295–326. — [`syntax-case`: procedural macros with automatic hygiene via syntax objects; the model for scope-annotated binding]
- Krishnamurthi, S. (2001). "Linguistic Reuse." Ph.D. thesis, Rice University. — [syntactic abstraction and parse-time hooks as the mechanism for language extensibility from user code]
