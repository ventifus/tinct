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
  [Macro      name: Str  params: Expr  body: Expr]   # params is a Let node
  [Placeholder]]

# flatten-args: re-extract the flat element sequence from a bracket that the
# parser has already interpreted (as a Call, Seq, Dict, or Let).
# Used by let-softening macros and any macro that needs bracket elements.
flatten-args: [fn [let node@Expr] -> Seq@Expr
  [match node
    [case [let p: Call]   [prepend p.func p.args]]  # [x y z] → Call(x,[y,z]) → [x,y,z]
    [case [let p: Let]    p.bindings]                # [let x y] → already flat
    [case [let p: Seq]    p.elements]                # [$x $y] → positional seq
    [case [let p: Dict]   [map [fn [let e] e.value] p.entries]]  # keyed dict → values
    [case [let _]         [list node]]]]             # scalar → one-element seq
```

Variant names match their keywords: `Fn` for `fn`, `Let` for `let`, `Case` for `case`, `Macro` for `macro`. No `Decl` suffix — there is only one `Let` and one `Macro` in the type.

`flatten-args` is the key helper for let-softening macros. It belongs in `stdlib/ast.llt` because it depends on the `Expr` type, but it is ordinary tinct code — not Rust infrastructure. Any user can write their own version or import it directly.

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

**Multi-arity dispatch.** When a macro needs to handle different argument counts, use `...args` variadic and `[match [length args] ...]` in the body. `[case ...]` appears inside `[match ...]` exactly as unified-bindings defines it — never at the outer macro declaration level:

```tinct
[macro my-and [let ...args@Seq@Expr]
  [match [length args]
    [case 0       [quote true]]
    [case 1       [first args]]
    [case 2
      [a: [first args]  b: [second args]]
      [quote [if [unquote a] [unquote b] false]]]
    [case [let _]
      [a: [first args]  rest: [rest args]]
      [quote [if [unquote a] [my-and [unquote-splice rest]] false]]]]]
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

### Let-Softening via `flatten-args`

When the parser processes `[fn [x y] body]`, the inner bracket `[x y]` becomes `Call(VarRef("x"), [VarRef("y")])` — implied-call semantics applied at parse time. A macro that wants to treat `[x y]` as a parameter list needs the flat element sequence back.

`flatten-args` (defined in `stdlib/ast.llt`) recovers it. The fn-softening macro is then ordinary tinct code — no special delivery mode, no parser hooks:

```tinct
# stdlib/syntax.llt — available to any program that opts in via [include %libdir "syntax.llt"]

[macro fn [let params@Expr  body@Expr]
  [match params
    [case [let _ : Let]   [quote [fn [unquote params] [unquote body]]]]
    [case [let _]
      [flat: [flatten-args params]]
      [quote [fn [let [unquote-splice flat]] [unquote body]]]]]]

[macro class [let tvars@Expr  ...body@Expr]
  [match tvars
    [case [let _ : Let]   [quote [class [unquote tvars] [unquote-splice body]]]]
    [case [let _]
      [flat: [flatten-args tvars]]
      [quote [class [let [unquote-splice flat]] [unquote-splice body]]]]]]

[macro type [let params@Expr  body@Expr]
  [match params
    [case [let _ : Let]   [quote [type [unquote params] [unquote body]]]]
    [case [let _]
      [flat: [flatten-args params]]
      [quote [type [let [unquote-splice flat]] [unquote body]]]]]]
```

`stdlib/syntax.llt` is not a privileged file. It is ordinary tinct code that happens to ship with the language. A user's own macro that does the same thing is indistinguishable from the stdlib version — `flatten-args` is available to everyone from `stdlib/ast.llt`. A user who wants softer syntax for their own DSL form writes exactly the same pattern, right in their own file.

A user who loads `stdlib/syntax.llt` can write `[fn [x@Int y@Float] body]`. A user who does not gets a type error from the type checker. The language's position is strict; the macro system makes it extensible.

**Pre-scan for registration.** `macro` declarations are scanned from the parsed AST before the transformation pass begins its first walk. This gives the pass a complete registry of registered form names before it processes any of them. Any form with a `macro` declaration automatically gets neutral key handling from the parser — no duplicate-key enforcement — since the macro body handles structural validation.

**Transformation to fixpoint.** The pass runs until no registered form names appear unvisited in the AST. A macro's output is re-visited. Depth limit 100 per site; total node-count cap 100k.

---

### `splice` — Multi-Form Output

A macro returns `[splice form1 form2 ...]` to inject multiple forms into the surrounding context:

```tinct
[macro derive [let ...targets@Seq@Expr  body@Expr]
  [splice
    ...[map [fn [let target]
              [quote [instance [unquote target] [unquote body]]]]
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
#   [match node [case [let _ : VarRef] ...] [case [let _ : Let] ...]]
#   [match node [case [let [name: n] : VarRef] ...]]   # extract VarRef's name field
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

### Simple 2: `my-or` — Multi-Arity Dispatch

Multi-arity is handled with `...args` variadic and `[match [length args] ...]` in the body. `[case ...]` appears inside `[match ...]` — the standard unified-bindings match arm form.

```tinct
[macro my-or [let ...args@Seq@Expr]
  [match [length args]
    [case 0       [quote false]]
    [case 1       [first args]]
    [case [let _]
      [a: [first args]  rest: [rest args]]
      [quote [if [unquote a] [unquote a] [my-or [unquote-splice rest]]]]]]]
```

The zero-arg case returns `[quote false]` — the AST for literal `false`. The one-arg case returns `[first args]` directly — it's already an AST node. The many-arg case builds new AST and recurses.

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
[macro fn [let params@Expr  body@Expr]
  [flat: [flatten-args params]]          # re-extract bracket elements
  [match [first-or flat Null]
    [case [let _ : Let]  [quote [fn [unquote params] [unquote body]]]]
    [case [let _]        [quote [fn [let [unquote-splice flat]] [unquote body]]]]]]
```

`flatten-args params` re-extracts the bracket elements — `Call(VarRef("x"), [VarRef("y")])` becomes `[VarRef("x"), VarRef("y")]`. `first-or flat Null` gets the first element. The `[case [let _ : Let]]` arm matches if params was already a `[let ...]` form. The wildcard catches everything else — `VarRef`, `Annotated`, `Null` (empty) — and wraps.

**Case 1 — already has `[let ...]`:** Idempotent; pass through unchanged.

```tinct
[fn [let x@Int y@Float] [+ x y]]
# params = Let(bindings: [Annotated(x,Int)  Annotated(y,Float)])
# flatten-args(Let(...)) → Let's bindings → [Annotated(x,Int) Annotated(y,Float)]
# first → Annotated → but wait: params itself is a Let → first-or flat = Annotated(x,Int)
# Hmm — actually: params IS the Let node; flatten-args returns its .bindings
# first of bindings → Annotated(x,Int) → wildcard... no. Let's re-read.
#
# Actually: params = Let(...) → flatten-args → returns p.bindings (a Seq@Expr)
# flat = [Annotated(x,Int)  Annotated(y,Float)]
# first-or flat Null → Annotated(x,Int) → wildcard arm fires → wrap?
#
# That would be wrong. The check should be: is params ITSELF a Let?
```

The `flatten-args` design above has a subtlety: for the pass-through case, we need to detect that `params` is already a `Let` node, not just that its first element is a `Let`. The match should be on `params` directly before calling `flatten-args`:

```tinct
[macro fn [let params@Expr  body@Expr]
  [match params
    [case [let _ : Let]                          # params itself is [let ...] — pass through
      [quote [fn [unquote params] [unquote body]]]]
    [case [let _]                                # params is Call/Seq/etc — unpack and wrap
      [flat: [flatten-args params]]
      [quote [fn [let [unquote-splice flat]] [unquote body]]]]]]
```

This is the correct form. Match on `params` first; call `flatten-args` only in the wrapping branch.

**Case 1 — already has `[let ...]`:** Idempotent; pass through unchanged.

```tinct
[fn [let x@Int y@Float] [+ x y]]
# params = Let(bindings: [...])  →  [case [let _ : Let]] matches  →  pass through
# → [fn [let x@Int y@Float] [+ x y]]  (unchanged)
```

**Case 2 — bare params, no annotations:**

```tinct
[fn [x y] [+ x y]]
# params = Call(VarRef("x"), [VarRef("y")])  →  wildcard arm
# flatten-args(Call) → [VarRef("x")  VarRef("y")]
# → [fn [let x y] [+ x y]]
```

**Case 3 — annotated params:**

```tinct
[fn [x@Int y@Float] [+ x y]]
# params = Call(Annotated(x,Int), [Annotated(y,Float)])  →  wildcard arm
# flatten-args(Call) → [Annotated(x,Int)  Annotated(y,Float)]
# → [fn [let x@Int y@Float] [+ x y]]
```

**Case 4 — empty params (`[fn [] body]`):**

```tinct
[fn [] body]
# params = Seq(elements: []) or Dict(entries: [])  →  wildcard arm
# flatten-args(Seq/Dict) → []  →  [unquote-splice []] splices nothing
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
[macro derive [let ...targets@Seq@Expr  body@Expr]
  [splice
    ...[map [fn [let target]
              [quote [instance [unquote target] [unquote body]]]]
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
[macro cond [let ...clauses@Seq@Expr]
  [match [length clauses]
    [case 0
      [quote [error "cond: no matching clause"]]]
    [case 1
      [clause: [first clauses]]
      [match [first clause]
        [case [let _ : VarRef]  [quote [unquote [second clause]]]]   # [else body]
        [case [let _]
          [quote [if [unquote [first clause]]
            [unquote [second clause]]
            [error "cond: no matching clause"]]]]]]
    [case [let _]
      [clause: [first clauses]  rest: [rest clauses]]
      [quote [if [unquote [first clause]]
        [unquote [second clause]]
        [cond [unquote-splice rest]]]]]]]
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
    [case 0
      [macro-error [span-of name] "pragma: name required"]]
    [case 1
      [match [get-or [first name] "type" null]
        [case [let _ : VarRef]
          [match value
            [case [let _ : Literal]  [quote [pragma [unquote [first name]] [unquote value]]]]
            [case [let _]            [macro-error [span-of value] "pragma value must be a literal"]]]]
        [case [let _]
          [macro-error [span-of [first name]] "pragma name must be a bare identifier"]]]]
    [case [let _]
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
**Proposed:** (1) A pre-scan pass collects `macro` declarations before the main parse begins, building a registry of form names. (2) Any form with a `macro` declaration in scope gets neutral key handling — the parser does not apply bare-name duplicate detection for that form's argument positions. (3) `fn`/`class`/`type` StackFrames accept any first sub-expression without error (semantic enforcement moved to type checker).
**Impact:** Moderate — pre-scan; neutral-key flag per form; StackFrame semantic checks removed.

### `src/expand.rs` — Parse-Stage Transformation Pass

**Current:** Expansion pass handles `defmacro` post-parse.
**Proposed:** Rename `defmacro` → `macro`. Add `splice` handling: when a macro returns `Expr::Splice(forms)`, inject into parent context. Update argument binding to use `[let ...]` pattern matching. No flat-list delivery mode — bracket unpacking is handled by `flatten-args` in the macro body itself.
**Impact:** Moderate — splice handling and `[let ...]` pattern binding; simpler than previously designed (no delivery mode infrastructure).

### `src/ast.rs` — New Variants

```rust
Expr::MacroDecl {
    name: String,
    params: Box<Spanned<Expr>>,   // Expr::LetDecl — same structure as fn params
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

Add: `span-of`, `wrap-in-let`, `let-decl-elems`, `first-or`, `prepend` (sequence helpers).
Add: `macro-error` as a Rust builtin (`ErrorKind::MacroError` with span).
Add: `stdlib/ast.llt` — defines `Expr`, `Annotation`, `Entry` nominal types plus `flatten-args`. No `ReceiveMode` or `MacroParam` (no delivery modes — the macro system is uniform). No predicate functions — variant pattern matching replaces them.
Update: `gensym` returns `VarRef(name: "prefix__N")` — a genuine `Expr` variant.
Update: `ast_to_dict` produces typed `Expr` variant values. All existing AST consumers migrate to variant pattern matching.
**Impact:** Minor — additive; simpler than prior design (no delivery mode infrastructure).

### `stdlib/syntax.llt` (new file)

`macro` declarations for fn/class/type let-softening, using `flatten-args` from `stdlib/ast.llt`. Available to any program that opts in via `[include %libdir "syntax.llt"]`. Not loaded by default — the core language remains strict without it. Mechanically identical to any user-written macro that imports `flatten-args` — `stdlib/syntax.llt` is not privileged.
**Impact:** New file, ~30 lines.

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
