# What If: Macro System v2 — Parse-Stage Delivery and Declarative Patterns for tinct

**State:** Proposal

What would it take to make tinct's macro system powerful enough that user-programmers — not language designers — can implement any syntactic extension, including softening the strong positions the core language takes?

## Current State

Tinct's macro system (`defmacro` + `quote`/`unquote`/`unquote-splice`) operates post-parse: macros receive fully-formed `Expr` AST dicts and return AST dicts. The expansion pass runs before type-checking. What is fully implemented:

- `defmacro` — procedural AST macros via `ast_to_dict` / `dict_to_ast`
- `quote` / `unquote` / `unquote-splice` — quasiquoting
- `gensym` — manual hygiene for macro-introduced names
- Provenance tracking — dual-span error reporting (macro call site + expansion site)
- `tmpl`, `do`, `begin` macros in stdlib

Two structural gaps remain.

**Argument destructuring is manual.** Every macro receives an opaque `args` sequence and must manually index into it — `[nth args 0]`, `[nth args 1]`, etc. There is no way to declare the expected shape of arguments or dispatch on argument count and structure. Every macro reimplements the same destructuring boilerplate, and callers get no structural error messages when they violate the macro's expectations.

```tinct
# Today: manual indexing, no structural validation
[defmacro my-if [args]
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

**Structural macros become writable in tinct.** `derive`-style macros — one invocation produces multiple `instance` declarations — require multi-form output. Pattern-matching dispatch forms — `[dispatch result [n@Int: ...] [n@String: ...]]` — require annotated keys to be distinct. Neither is expressible today. Both fall out directly from this proposal.

**Macro errors become precise.** Today, a macro that receives wrong-shaped arguments either crashes at runtime with a confusing index error or produces malformed AST that fails at type-check with no connection to the macro call. `macro-error` and `span-of` let macros produce errors that point at the exact source location of the problem.

## Design

### The Architecture

The key invariant is the three-layer pipeline:

```
source → [parse: syntactic only] → [transformation pass: user macros] → [type-check: semantic enforcement] → eval
```

**The parser** handles syntax: bracket nesting, token classification, form recognition. It does not enforce semantic rules — that a `fn` parameter list must be `Expr::LetDecl`, that `[let ...]` must appear in binding positions. The parser accepts `[fn anything body]` and produces `FnExpr { params: <whatever>, body: ... }`. It never hard-errors on semantic mismatches.

**The transformation pass** runs next. User macros see the parser's output and reshape it. A `defparse-macro fn` macro intercepts `FnExpr` nodes whose params are not `Expr::LetDecl` and wraps them. The type checker then sees conforming code.

**The type checker** enforces semantic rules. `FnExpr` with non-`LetDecl` params → type error. `[let ...]` absent from a binding position → type error. Semantic enforcement belongs here, not in the parser.

This is correct layering independently of macros. Macros benefit because they occupy the right slot in an already-correct pipeline: the transformation pass runs before semantic enforcement fires.

**Consequence for unified-bindings:** anywhere `doc/whatif/unified-bindings.md` currently states "parse error" for a missing `[let ...]`, this is a type error. The parser StackFrames for `fn`, `class`, `type` accept any first sub-expression; the type checker rejects non-`LetDecl` params. Nothing about the parser architecture changes for macros specifically — it was always wrong to put semantic enforcement in the parser.

---

### `defmacro` with `[let ...]` Argument Patterns

The existing `defmacro` form accepts `[let ...]` patterns in the argument position — the same syntax as function parameters. Pattern variables bind to the corresponding argument positions. No new syntax; `[let ...]` is already the universal binding form.

```tinct
# Before: manual indexing
[defmacro my-if [args]
  [cond: [nth args 0]
   then: [nth args 1]
   else: [nth args 2]]
  [list 'if cond then else]]

# After: [let ...] pattern
[defmacro my-if [let cond then else]
  [list 'if cond then else]]
```

Typed patterns constrain expected shapes:

```tinct
[defmacro my-assert [let condition@Expr  message@Str]
  [list 'if condition true [list 'error message]]]
```

Variadic via `...rest` — already defined in `[let ...]` for function params:

```tinct
[defmacro my-list [let ...items]
  [reduce [fn [let acc item] [list 'cons item acc]] [list 'null] items]]
```

**Multi-arm dispatch.** When a macro needs to handle different argument shapes, `[case ...]` arms dispatch on argument count and structure — the same match syntax:

```tinct
[defmacro my-and
  [[case [let a]]          a]
  [[case [let a b]]        [list 'if a b false]]
  [[case [let a ...rest]]  [list 'if a [list 'my-and ...rest] false]]]
```

**Hygiene.** With `[let ...]` patterns, the template/user-code distinction is structural:
- Names bound in the `[let ...]` argument pattern are *user-code bindings* — they hold pieces of the caller's input AST. No rename needed; they are the user's own names.
- Names introduced by `gensym` in the body are *macro-introduced* — they must not capture caller-scope variables.

```tinct
[defmacro with-retry [let max-attempts body]
  [tmp: [gensym "count"]]
  [list 'let [list tmp 0]
    [list 'while [list '< tmp max-attempts]
      body
      [list 'set! tmp [list '+ tmp 1]]]]]
```

`tmp` is gensym'd — macro-introduced. `max-attempts` and `body` are from the pattern — user-provided. The distinction is syntactically explicit. Scope set activation (Phase 2) becomes straightforward: pattern-bound names carry the caller's scope; gensym names carry the macro's scope.

---

### `defparse-macro` — Pre-Call-Semantics Argument Delivery

For cases where the parser's call-semantics interpretation loses structure the macro needs, `defparse-macro` declares a receive mode per argument position:

```tinct
[defparse-macro name [arg: receive-mode  ...] body]
```

**Receive modes:**

| Mode | What the macro receives | When to use |
|------|------------------------|-------------|
| `expr` | Fully parsed expression (default) | Standard — same as `defmacro` |
| `flat-list` | Bracket elements as a sequence, before call-semantics | When element structure matters more than call interpretation |

`flat-list` is the key mode. `[x@Int y@Float]` parses as `Call(Annotated("x", Int), [Annotated("y", Float)])` in `expr` mode — the flat sequence is gone. In `flat-list` mode, the transformation pass extracts the bracket's entries and delivers `[Annotated("x", Int), Annotated("y", Float)]` as a tinct `Seq`, regardless of how the parser interpreted the bracket form.

**The fn let-softening macro:**

```tinct
# stdlib/syntax.llt — available to any program that opts in
[defparse-macro fn [params: flat-list  body: expr]
  [if [let-decl? [first params]]
    [list 'fn params body]                  # already [let ...], pass through
    [list 'fn [cons 'let params] body]]]    # prepend let

[defparse-macro class [tvars: flat-list  ...body: expr]
  [if [let-decl? [first tvars]]
    [list 'class tvars ...body]
    [list 'class [cons 'let tvars] ...body]]]

[defparse-macro type [params: flat-list  body: expr]
  [if [let-decl? [first params]]
    [list 'type params body]
    [list 'type [cons 'let params] body]]]
```

Each macro body is pure tinct. No Rust flags. No hardcoded transformation logic. The infrastructure provides delivery; all logic lives in the macro.

A user who loads `stdlib/syntax.llt` can write `[fn [x@Int y@Float] body]`. A user who does not gets a type error from the type checker. The language's position is strict; the macro system makes it extensible. The user — not the language designer — controls which ergonomic forms are available in their codebase.

**How the pass delivers `flat-list`.** For a registered form name, the transformation pass extracts the bracket's entries from whatever AST node the parser produced:
- From a `Call` node: `[func, arg0, arg1, ...]` (func + all args as elements)
- From a `Dict` node: the dict entries
- From an `Expr::LetDecl` node: the bindings (already flat — pass through unchanged)

The resulting `Seq` is delivered to the macro body. The macro inspects and reshapes it, returning a new AST dict. The pass substitutes the original form with the result and continues.

**Pre-scan for registration.** `defparse-macro` and `declare-key-identity` declarations are scanned from the parsed AST before the transformation pass begins its first walk. This gives the pass a complete registry of registered form names before it processes any of them. Stdlib declarations in `stdlib/syntax.llt` are always pre-loaded.

**Transformation to fixpoint.** The pass runs until no registered form names appear unvisited in the AST. A macro's output is re-visited. Depth limit 100 per site; total node-count cap 100k.

---

### `declare-key-identity` — Annotated Keys in Macro Forms

Duplicate detection fires during parsing, before any macro runs. By default, `n@Int` and `n@String` both resolve to bare key `"n"` — a parse-time duplicate error, before the macro ever sees them.

`declare-key-identity` is scanned before parsing begins and registers a form name with the parser to use full-expression equality for duplicate detection in that form's body:

```tinct
[declare-key-identity dispatch  full-expression]
# n@Int ≠ n@String ≠ n — all three can coexist as distinct keys
```

| Identity | Behavior |
|----------|----------|
| `bare-name` (default) | `n@Int` and `n@String` → key `"n"` → duplicate |
| `full-expression` | Structural comparison — `n@Int` and `n@String` are distinct |

Two `n@Int` entries are still a duplicate. Two `_` entries are still a duplicate. Only the annotation distinguishes them.

```tinct
[declare-key-identity dispatch  full-expression]

[defmacro dispatch [let scrutinee  ...arms]
  [list 'match scrutinee
    ...[map [fn [let arm]
              [list 'case [first arm] [second arm]]]
           arms]]]

# Usage — n@Int and n@String are distinct arms:
[dispatch result
  [Ok v]:    [process v]
  [Err msg]: [log-error msg]
  n@Int:     [str "int: " n]
  n@String:  [str "str: " n]]
```

---

### `splice` — Multi-Form Output

A macro returns `[splice form1 form2 ...]` to inject multiple forms into the surrounding context:

```tinct
[defmacro derive [targets: flat-list  ...body: expr]
  [splice
    ...[map [fn [let target]
              [list 'instance target ...body]]
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
[defparse-macro pragma [name: flat-list  value: expr]
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
# Inspection
[let-decl? expr]    # is this Expr::LetDecl?
[var-ref? expr]     # is this a bare identifier?
[annotated? expr]   # is this name@Type?
[literal? expr]     # is this a literal scalar value?
[call? expr]        # is this a function call form?
[span-of expr]      # extract source span

# Construction
[list 'sym a b c]        # construct AST form [sym a b c]
[cons x xs]              # prepend x to sequence
[first xs] [rest xs]     # sequence access
[gensym prefix]          # fresh unique identifier
[macro-error span msg]   # compile-time error

# Stdlib helpers
[wrap-in-let elems]      # produce [let ...elems]
[let-decl-elems decl]    # extract bindings from Expr::LetDecl
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

---

### Simple 1: `unless` — Single `[let ...]` Pattern

```tinct
[defmacro unless [let cond body]
  [list 'if cond [list] body]]
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
[defmacro my-or
  [[case [let]]            false]
  [[case [let a]]          a]
  [[case [let a b]]        [list 'if a a b]]
  [[case [let a ...rest]]  [list 'if a a [cons 'my-or rest]]]]
```

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
[defmacro with-tmp [let expr body]
  [tmp: [gensym "tmp"]]
  [list 'let [list [list tmp expr]] body]]
```

```tinct
[with-tmp [expensive-computation] [+ tmp 1]]
# → [let [[tmp__42: [expensive-computation]]] [+ tmp__42 1]]
```

**Edge case — user variable named `tmp`:** Without gensym, the macro would introduce `tmp` and shadow the user's own `tmp`. Gensym produces `tmp__42` (a name containing `:` or `__` that cannot appear in user-written tinct), so the user's `tmp` is unaffected.

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
[defparse-macro fn [params: flat-list  body: expr]
  [if [or [empty? params] [let-decl? [first params]]]
    [list 'fn params body]
    [list 'fn [cons 'let params] body]]]
```

**Case 1 — already has `[let ...]`:** Idempotent; pass through unchanged.

```tinct
[fn [let x@Int y@Float] [+ x y]]
# params flat-list: [LetDecl([x@Int y@Float])]
# [let-decl? [first params]] → true
# → [fn [let x@Int y@Float] [+ x y]]  (unchanged)
```

**Case 2 — bare params, no annotations:**

```tinct
[fn [x y] [+ x y]]
# params flat-list: [VarRef("x") VarRef("y")]
# [let-decl? [first params]] → false
# → [fn [let x y] [+ x y]]
```

**Case 3 — annotated params:**

```tinct
[fn [x@Int y@Float] [+ x y]]
# params flat-list: [Annotated("x", Int) Annotated("y", Float)]
# first is Annotated, not LetDecl → wrap
# → [fn [let x@Int y@Float] [+ x y]]
```

**Case 4 — empty params (`[fn [] body]`):**

```tinct
[fn [] body]
# params flat-list: [] (empty)
# [empty? params] → true → pass through
# → [fn [] body]   (or [fn [let] body] — either is valid for zero-param fn)
```

**Case 5 — variadic params:**

```tinct
[fn [f ...args] [map f args]]
# params flat-list: [VarRef("f") Spread(VarRef("args"))]
# first is VarRef, not LetDecl → wrap
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
[defmacro derive [targets: flat-list  ...body: expr]
  [splice
    ...[map [fn [let target]
              [list 'instance target ...body]]
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

### Complex 3: `dispatch` — Annotated Keys

```tinct
[declare-key-identity dispatch  full-expression]

[defmacro dispatch [let scrutinee  ...arms]
  [list 'match scrutinee
    ...[map [fn [let arm]
              [list 'case [first arm] [second arm]]]
           arms]]]
```

```tinct
[dispatch result
  [Ok v]:    [process v]
  [Err msg]: [log-error msg]
  n@Int:     [str "int: " n]
  n@String:  [str "str: " n]]
```

Under `full-expression` identity, `n@Int` and `n@String` are structurally distinct keys — no duplicate error. Expands to:

```tinct
[match result
  [case [Ok v]    [process v]]
  [case [Err msg] [log-error msg]]
  [case n@Int     [str "int: " n]]
  [case n@String  [str "str: " n]]]
```

**Edge case — two identical annotated keys:**

```tinct
[dispatch result
  n@Int: "first"
  n@Int: "second"]   # parse error: duplicate key n@Int
```

Two `n@Int` entries are still a parse-time duplicate even under `full-expression` identity. Only the annotation distinguishes them; identical annotations still collide.

**Edge case — bare name alongside annotated name:**

```tinct
[dispatch result
  n@Int: [str "int: " n]
  n:     "fallback"]   # valid — n@Int ≠ n under full-expression identity
```

---

### Complex 4: `pragma` — `macro-error` for Structural Validation

```tinct
[defparse-macro pragma [name: flat-list  value: expr]
  [match [length name]
    [[case 0]
      [macro-error [span-of name]  "pragma: name required"]]
    [[case 1]
      [if [not [var-ref? [first name]]]
        [macro-error [span-of [first name]] "pragma name must be a bare identifier"]
        [if [not [literal? value]]
          [macro-error [span-of value]       "pragma value must be a literal"]
          [list 'pragma [first name] value]]]]
    [[case [let _]]
      [macro-error [span-of name]  "pragma: exactly one name allowed"]]]]
```

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
**Proposed:** Corrected to "type error." The parser accepts `[fn any-expr body]`; the type checker enforces `Expr::LetDecl` in param position. This removes hard enforcement from StackFrames for `fn`, `class`, `type` and places it in `check_fn_expr` in `src/typecheck.rs`.
**Impact:** Minor in scope; architectural correctness improvement independent of macros.

### `src/parser.rs` — Pre-Scan and Key Identity

**Current:** Duplicate detection uses bare-name identity unconditionally. `fn`/`class`/`type` StackFrames enforce `Expr::LetDecl`.
**Proposed:** (1) A pre-scan pass over the token stream collects `declare-key-identity` and `defparse-macro` declarations before the main parse begins. `declare-key-identity` registrations switch the named form's body to full-expression duplicate detection. (2) `fn`/`class`/`type` StackFrames accept any first sub-expression without error.
**Impact:** Moderate — pre-scan and key-identity dispatch; StackFrame semantic checks removed.

### `src/expand.rs` — Parse-Stage Transformation Pass

**Current:** Expansion pass handles `defmacro` post-parse.
**Proposed:** Extend the pass with `defparse-macro` support: re-deliver arguments per declared receive modes, call macro body, substitute result. Add `flat-list` delivery: extract bracket entries from the parsed AST node (Call args, Dict entries, LetDecl bindings). Add `splice` handling: when a macro returns `Expr::Splice(forms)`, inject into parent context. Update `defmacro` argument binding to use `[let ...]` pattern matching.
**Impact:** Moderate — new delivery logic and splice handling; integrates with existing expansion infrastructure.

### `src/ast.rs` — New Variants

```rust
Expr::ParseStageMacroDecl {
    name: String,
    params: Vec<(String, ReceiveMode)>,
    body: Box<Spanned<Expr>>,
}

Expr::KeyIdentityDecl { form: String, identity: KeyIdentity }

Expr::Splice(Vec<Spanned<Expr>>)
```

**Impact:** Minor — three new variants; exhaustive match arms updated mechanically.

### `src/typecheck.rs` — Semantic Enforcement for Binding Positions

**Current:** `fn`/`class`/`type` param checking happens in the parser.
**Proposed:** Move to `check_fn_expr`, `check_class_decl`, `check_type_alias`: if the first sub-expression is not `Expr::LetDecl`, emit type error "parameter list must be a `[let ...]` binding declaration."
**Impact:** Minor — enforcement moved, semantics unchanged.

### `stdlib/prelude.llt` — New AST Primitives

Add: `let-decl?`, `var-ref?`, `annotated?`, `literal?`, `call?`, `span-of`, `wrap-in-let`, `let-decl-elems`.
Add: `macro-error` as a Rust builtin (`ErrorKind::MacroError` with span).
**Impact:** Minor — additive.

### `stdlib/syntax.llt` (new file)

`defparse-macro` declarations for fn/class/type let-softening, available to any program that opts in via `[include %libdir "syntax.llt"]`. Not loaded by default — the core language remains strict without it.
**Impact:** New file, ~50 lines.

---

## Prerequisites

- **`defmacro`** — fully implemented; this proposal extends it
- **`unified-bindings`** (`Expr::LetDecl`) — `let-decl?` requires it; the corrected parser enforcement requires the unified-bindings parser update to not hard-error on missing `[let ...]`
- **`ast_to_dict` / `dict_to_ast`** — already implemented (`ast-dict-core`); macro bodies use them

## References

- Tobin-Hochstadt, S. et al. (2011). "Languages as Libraries." *PLDI '11*, pp. 132–141. ACM. — [`#lang` as user-defined language extension via macros; the principle that language positions should be extensible from user code, not only from language designers]
- Flatt, M. (2016). "Binding as sets of scopes." *POPL '16*, pp. 705–717. ACM. — [scope sets for hygienic macro expansion; the formal model for distinguishing macro-introduced bindings from user-code bindings]
- Flatt, M. & PLT (2010). "Reference: Racket." §Syntax Classes (`syntax-parse`). — [declarative pattern declarations for macro arguments as the ergonomic foundation of the macro system; model for `[let ...]` patterns in `defmacro`]
- Graham, P. (1993). *On Lisp.* Prentice Hall. — [macros as functions from code to code; macro body as ordinary program; the principle that all macro logic should live in the macro, not the infrastructure]
- Kohlbecker, E., Friedman, D.P., Felleisen, M. & Duba, B. (1986). "Hygienic Macro Expansion." *LFP '86*, pp. 151–161. ACM. — [first formal definition of hygiene; gensym as minimal hygiene guarantee; structural impossibility of collision]
- Dybvig, R.K., Hieb, R. & Bruggeman, C. (1993). "Syntactic Abstraction in Scheme." *Lisp and Symbolic Computation*, 5(4), 295–326. — [`syntax-case`: procedural macros with automatic hygiene via syntax objects; the model for scope-annotated binding]
- Krishnamurthi, S. (2001). "Linguistic Reuse." Ph.D. thesis, Rice University. — [syntactic abstraction and parse-time hooks as the mechanism for language extensibility from user code]
