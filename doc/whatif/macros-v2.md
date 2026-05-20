# What If: Macro System v2 — Parse-Stage Delivery and Declarative Patterns for tinct

**State:** Accepted — 2026-05-17
**Replaces:**
- [`parse-stage-macros.md`](parse-stage-macros.md) — supersedes the parse-stage argument delivery approach
- [`completed/macro-rewrite.md`](completed/macro-rewrite.md) — supersedes the defmacro-as-desugaring approach

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

**The transformation pass** runs next. User macros see the parser's output and reshape it. A `macro fn [let params@Expr body@Expr]` macro (using `flatten-args`) intercepts `FnExpr` nodes whose params are not `Expr::Let` and wraps them. The type checker then sees conforming code.

**The type checker** enforces semantic rules. `FnExpr` with non-`Let` params → type error. `[let ...]` absent from a binding position → type error. Semantic enforcement belongs here, not in the parser.

This is correct layering independently of macros. Macros benefit because they occupy the right slot in an already-correct pipeline: the transformation pass runs before semantic enforcement fires.

**Consequence for unified-bindings:** anywhere `doc/whatif/unified-bindings.md` currently states "parse error" for a missing `[let ...]`, this is a type error. The parser StackFrames for `fn`, `class`, `type` accept any first sub-expression; the type checker rejects non-`Let` params. Nothing about the parser architecture changes for macros specifically — it was always wrong to put semantic enforcement in the parser.

---

### AST Types

Macros receive and return values of type `Expr` — a nominal variant type defined in `stdlib/ast.llt` and produced by `ast_to_dict`. This is the same `Expr` tinct's own evaluator works with, exposed as a first-class tinct type. Dispatch on AST node kind is structural pattern matching on the `Expr` variant — the same `[match ...]`/`[case ...]` syntax used everywhere else in tinct.

```tinct
# stdlib/ast.llt

[type Entry
  [KeyedEntry   key: Str  value: Expr]
  [UnkeyedEntry value: Expr]]

[type Annotation
  [Simple   name: Str]           # @Int, @Bool
  [PropDict entries: [Seq Entry]]  # @[return: T  constraint: ...]
  Null]                          # no annotation

[type Expr
  [Let        bindings: [Seq Expr]]
  [Case       pattern: Expr  body: Expr]
  [VarRef     name: Str]
  [Call       func: Expr  args: [Seq Expr]]
  [Annotated  expr: Expr  ann: Annotation]
  [Literal    value: Top]                     # Top: any scalar (Int, Float, Str, Bool, Null)
  [Dict       entries: [Seq Entry]]             # keyed dict
  [Seq        elements: [Seq Expr]]             # positional sequence
  [Fn         ann: Annotation  params: Expr  body: Expr]  # params is semantically a Let
  [Macro      name: Str  params: Expr  body: Expr]        # params is semantically a Let
  [Quote      expr: Expr]
  [Unquote    expr: Expr]
  [UnquoteSplice expr: [Seq Expr]]
  [Splice     forms: [Seq Expr]]
  [Placeholder]]

# flatten-args: re-extract the flat element sequence from a bracket that the
# parser has already interpreted (as a Call, Seq, Dict, or Let).
# Used by let-softening macros and any macro that needs bracket elements.
flatten-args: [fn [let node@Expr] -> [Seq Expr]
  [match node
    [case [let p: Call]   [cons p.func p.args]]   # [x y z] → Call(x,[y,z]) → [x,y,z]
    [case [let p: Let]    p.bindings]              # [let x y] → already flat
    [case [let p: Seq]    p.elements]              # [$x $y] → positional seq
    [case [let p: Dict]   [map [fn [let e] e.value] p.entries]]  # empty [] or keyed dict → values
    [case [let _]         [list node]]]]           # scalar → one-element seq

# Note: the Dict arm is primarily for the empty bracket [] (an empty Dict in the parser).
# A non-empty keyed dict [x: 1 y: 2] extracts only values, losing keys — but keyed dicts
# are not valid parameter lists anyway; the type checker will reject them.
```

Variant names match their keywords. `Macro.params` and `Fn.params` are both typed as `Expr` in the AST, constrained semantically to `Let` by the type checker after macro expansion — the parser is permissive, the type checker enforces.

`flatten-args` belongs in `stdlib/ast.llt` because it depends on the `Expr` type. It is ordinary tinct code — not Rust infrastructure. Any user can write their own version.

**`gensym`** takes a prefix string and returns a `String` in `:prefix:N` format — the `:` separator is in the lexer's denylist, guaranteeing structural impossibility of collision (Kohlbecker 1986). Callers construct a `VarRef` AST node from the string using `do-var-node` or equivalent when an identifier is needed:

```tinct
[gensym "counter"]   # → ":counter:42"  (String)
[gensym "tmp"]       # → ":tmp:43"      (String)
[do-var-node [gensym "tmp"]]  # → VarRef(name: ":tmp:43")
```

---

### `inject:` — Anaphoric Name Injection

Hygienic macros use `gensym` for names the caller should never reference. The opposite case — names the caller *must* reference — requires `inject:`. A macro declares `inject: name` to intentionally make `name` available in the caller's scope (Nim calls this "inject"; Kohlbecker 1986 calls it "breaking hygiene deliberately").

```tinct
[macro aif [let test@Expr  then@Expr  else@Expr]
  inject: it       # "it" is the default injected name; caller overrides via dict key
  [quote [let [[[unquote binding]: [unquote test]]]
    [if [unquote binding] [unquote then] [unquote else]]]]]
```

`inject: it` does three things:
1. Sets the **default binding name** (`it`) — used when the macro is called in expression position.
2. Binds **`binding`** implicitly in the macro body — a `VarRef` holding the actual name in use. In expression position: `VarRef("it")`; in dict-key position: the caller's key.
3. Enables **dict-key override** — when called as `user: [aif ...]`, the expander uses `user` as the binding name instead of `it`. The caller can always rename by writing a dict key. When called without a key, `it` is used.

```tinct
# Default name — "it" in scope inside then/else
[aif [find-user id]
  [log "found" name: it.name]
  [error "not found"]]

# Caller overrides via dict key — "user" in scope
user: [aif [find-user id]
  [log "found" name: user.name]
  [error "not found"]]
```

The expander threads the dict key by injecting it into the `let` binding inside `aif`'s expansion — breaking the circular reference that would otherwise occur in tinct's letrec dict semantics (`user` bound to the `aif` result would circularly depend on itself if `user` appeared in a body argument). The internal `let` establishes `user` as the test result before any body argument is evaluated.

**Anaphoric macros cannot be called anonymously** — if a macro declares `inject:` and is called in expression position without a dict key, the injected name falls back to the `inject:` default. This is always valid.

**`[macro-injects name]`** — reflection primitive; returns the `inject:` default as a `Str`, or `Null` if the macro does not declare `inject:`.

```tinct
[macro-injects aif]   # → "it"
[macro-injects swap]  # → null (gensym-hygienic, no injected name)
```

**`inject:` is not `gensym`.** Use `gensym` for names the caller should never reference (internal temporaries). Use `inject:` for names the caller must reference. The two cover all cases; there is no third kind.

**`[quote expr]`** returns `Expr` — the AST node for `expr`. **`[unquote val]`** requires `val : Expr`; the type checker enforces this at each `[unquote ...]` site. A macro body must return `Expr`; returning any other type is a type error detected at macro definition time.

**Materialization boundary.** Macro bodies run in the tinct evaluator, but the expander deep-materializes both the input AST (before calling the macro body) and the output (before passing to `dict_to_ast`). This means: (1) all Expr fields delivered to the macro body are already forced — no laziness at the input boundary; (2) the macro body's return value must be fully materializable — non-terminating thunks will hang the compiler. Lazy sequences (`Seq` from `$range` etc.) cannot be returned from a macro body.

Macro bodies that annotate their parameters get full type checking:

```tinct
[macro my-if [let cond@Expr  then@Expr  else@Expr]
  [quote [if [unquote cond] [unquote then] [unquote else]]]]
```

Unannotated macro parameters receive a fresh `TypeVar` — same as unannotated `fn` parameters in HM inference. The type checker unifies the TypeVar against how the parameter is used in the body. Annotating with `@Expr` is equivalent to stating the constraint explicitly and enables structural match validation.

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
[macro my-assert [let condition@Expr  message@Literal]
  # message@Literal: the argument must be a string literal AST node, not a runtime Str value.
  # Macro params always receive Expr nodes; @Literal constrains which Expr variant is valid.
  [quote [if [unquote condition] true [error [unquote message]]]]]
```

Variadic via `...rest` — already defined in `[let ...]` for function params:

```tinct
[macro my-list [let ...items]
  [quote [list [unquote-splice items]]]]
```

**Multi-arity dispatch.** When a macro needs to handle different argument counts, use `...args` variadic and `[match [length args] ...]` in the body. `[case ...]` appears inside `[match ...]` exactly as unified-bindings defines it — never at the outer macro declaration level:

```tinct
[macro my-and [let ...args@[Seq Expr]]
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
# swap: exchange two values, evaluating each exactly once.
# gensym is necessary to avoid capturing the caller's `tmp` variable.
[macro swap [let a@Expr  b@Expr]
  [tmp: [gensym "tmp"]]
  [quote [let [[[unquote tmp]: [unquote a]]]
    [pair [unquote b] [unquote tmp]]]]]
```

```tinct
# Without gensym — wrong if caller has a variable named "tmp":
[let [tmp: 99]
  [swap tmp [+ tmp 1]]]
# Bad expansion: [let [[tmp: tmp]] [pair [+ tmp 1] tmp]]
# "tmp" in the let binding captures the caller's 99 — shadowing

# With gensym — safe:
# Expansion: [let [[:tmp:42: tmp]] [pair [+ tmp 1] :tmp:42]]
# :tmp:42 can never be written by the user (: is in the denylist)
```

`tmp` is gensym'd — macro-introduced, named `:tmp:42`. `a` and `b` are from the `[let ...]` pattern — user-provided. `gensym` returns the String `":tmp:42"`; `do-var-node` wraps it in a `VarRef` AST node so `[unquote tmp]` splices the fresh identifier directly in binding position and reference position.

**Hygiene is complete without scope sets.** In tinct's macro system, every name in a macro body is either (a) pattern-bound from user input — inherently in user scope, never capturing anything — or (b) gensym'd with `:prefix:N` naming — structurally unforgeable, never written by users. There is no third category where a macro could accidentally introduce a name that captures user scope. Scope sets (Flatt 2016) solve that third-category problem; tinct's design eliminates the category. Manual gensym with unforgeable names is both necessary and sufficient.

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

**Transformation to fixpoint.** The expander walks the AST depth-first, re-expanding macro call outputs immediately. Expansion is bounded by a **total expansion depth limit of 100** (not per-site — the counter is shared across all expansion in a file) and a total node-count cap of 100k. These limits are practical bounds against infinite recursion; expansion termination is undecidable for procedural macros (Dybvig 1993). Both limits are configurable. The `Splice` variant is an expansion-time error if it appears in expression position — the expander checks this before continuing the walk.

---

### `splice` — Multi-Form Output

A macro returns `[splice form1 form2 ...]` to inject multiple forms into the surrounding context:

```tinct
[macro derive [let ...targets@[Seq Expr]  body@Expr]
  [splice
    ...[map [fn [let target]
              [quote [instance [unquote target] [unquote body]]]]
           targets]]]

# Usage:
@[derive Equal Comparable]
Point: [type [x@Float  y@Float]]
# Expands to: [instance Equal ...]  [instance Comparable ...]  Point: [type ...]
```

At dict top level, each spliced form becomes a separate dict entry, each participating in the enclosing dict's letrec scope. In expression position, `splice` is an **expansion-time error** raised by the expander — `Splice` is an `Expr` variant and therefore invisible to the type checker, so enforcement must happen during the transformation pass before type-checking begins.

---

### `macro-error` and `span-of` — Compile-Time Error Signaling

Macro bodies signal structured compile-time errors that point at source locations:

```tinct
[macro-error span message]   # terminate transformation with compile error at span
[span-of expr]               # extract source span from an AST node
```

```tinct
[macro pragma [let name@Expr  value@Expr]
  [name-seq: [flatten-args name]]
  [match [length name-seq]
    [case 0  [macro-error [span-of name] "pragma name must be a single bare identifier"]]
    [case 1
      [match [first name-seq]
        [case [let _ : VarRef]
          [match value
            [case [let _ : Literal]  [quote [pragma [unquote [first name-seq]] [unquote value]]]]
            [case [let _]            [macro-error [span-of value] "pragma value must be a literal"]]]]
        [case [let _]  [macro-error [span-of name] "pragma name must be a bare identifier"]]]]
    [case [let _]  [macro-error [span-of name] "pragma: exactly one name allowed"]]]]
```

`macro-error` raises a `MacroError` (a new `ErrorKind` variant) at the given span. It is surfaced before type-checking with the same formatting as other compile errors. When a macro body throws a runtime error (not via `macro-error`), the expander wraps it with `macro_expansion` provenance — the error includes both the definition-site location and the call-site span, so the user sees where in the macro body the failure occurred.

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
[unquote-splice seq]     # splice a [Seq Expr] into the enclosing quote

# Sequence operations on flat-list deliveries ([Seq Expr])
[first xs] [rest xs]     # element access
[first-or xs default]    # first element, or default if empty

# Gensym and error
[gensym prefix@Str]      # returns String ":prefix:N" — structurally unforgeable fresh name; wrap with do-var-node for VarRef
[macro-error span msg]   # terminate transformation with MacroError at span (expansion-time, not type-check-time)

# Stdlib helpers
[wrap-in-let elems]      # produce Let(bindings: elems) AST node
[let-decl-elems decl]    # extract bindings [Seq Expr] from a Let node
[ident str@Str]          # construct VarRef(name: str) from a string — the counterpart to
                         # gensym for EXISTING names (str must already be a valid identifier)
```

---

### Syntax Classes — Structural Validation at the Call Site

When a macro parameter is annotated with a specific `Expr` variant (e.g., `name@VarRef`), the expander validates the argument before calling the macro body. If validation fails, the error is reported at the call site with a structural description — not as a runtime failure deep inside the macro body.

```tinct
# @VarRef on a param: expander validates arg is a VarRef before body runs
[macro pragma [let name@VarRef  value@Literal]
  [quote [pragma [unquote name] [unquote value]]]]

[pragma optimize true]    # ✓ — optimize is VarRef, true is Literal
[pragma [+ 1 2] true]     # expansion error at call site:
                          #   "pragma: argument 'name' expected VarRef, got Call"
[pragma optimize x]       # expansion error:
                          #   "pragma: argument 'value' expected Literal, got VarRef"
```

`name@VarRef` binds the full `VarRef` node (not just its payload) to `name`. `[unquote name]` splices the VarRef identifier directly. To access the name string: `name.name : Str`. To reconstruct a VarRef from a computed string: `[ident computed-str]`.

**Named syntax classes** provide custom error messages. A `syntax-class` declaration combines a pattern with a human-readable description:

```tinct
[syntax-class pragma-name
  pattern: [let _ : VarRef]
  message: "bare identifier (e.g., optimize, debug)"]

[syntax-class pragma-value
  pattern: [let _ : Literal]
  message: "literal value (e.g., true, 42, \"fast\")"]

[macro pragma [let name@pragma-name  value@pragma-value]
  [quote [pragma [unquote name] [unquote value]]]]

[pragma [+ 1 2] true]
# "pragma: argument 'name' — expected bare identifier (e.g., optimize, debug), got Call"
```

Named syntax classes can be reused across multiple macros. The `pattern:` field is a `[let ...]` binding pattern — the same syntax used everywhere in tinct. The `message:` field is the user-facing description of what the validator expects.

**Syntax class attributes.** `arg@SyntaxClass` binds `arg` to the full matched `Expr` node. Payload fields are accessed via dot notation using the Expr type's actual field names — not pattern aliases. For `arg@VarRef`, `arg.name : Str` gives the identifier name. For `arg@Annotated`, `arg.expr : Expr` and `arg.ann : Annotation` give the payload fields. If you want shorter names in the body, bind them explicitly:

```tinct
[syntax-class var-with-type
  pattern: [let _ : Annotated]
  message: "annotated expression (e.g., x@Int)"]

[macro log-type [let arg@var-with-type]
  # arg is the full Annotated Expr node; access fields by actual name
  [e: arg.expr  a: arg.ann]
  [quote [log [str "type of " [unquote [ident e.name]] " is " [unquote [to-str a]]]]]]
```

`[ident str]` constructs a `VarRef` for an existing name — here `e.name` is the VarRef payload's name string, and `[ident e.name]` recreates the identifier. Distinct from `gensym` (fresh unforgeable name): `ident` reconstructs names the user wrote.

**Variadic params with syntax class validation.** `...args@[Seq VarRef]` collects all remaining arguments into `args` and validates each element is a `VarRef`. The expander checks every element; if any fails, the error identifies which position failed:

```tinct
[macro define-accessors [let type-name@VarRef  ...fields@[Seq VarRef]]
  ...]

[define-accessors Point  x  y  z]    # ✓ — x, y, z are all VarRef
[define-accessors Point  x  [+ 1 2]] # expansion error: argument 2 in 'fields'
                                      #   expected VarRef, got Call
```

---

### Meta-Macros — Macros Generating Macros

A macro can produce `[macro ...]` declarations in its output (via `splice`). When the expander encounters a `MacroDecl` node in expansion output, it registers the new macro immediately and continues expanding. Subsequent forms in the same dict can use the newly registered macro.

```tinct
# A meta-macro that generates one accessor macro per field
[macro define-accessors [let type-name@VarRef  ...fields@[Seq Expr]]
  [splice
    ...[map [fn [let field@VarRef]
              [quote [macro [unquote field] [let obj@Expr]
                [quote [get [unquote obj] [unquote [ident field.name]]]]]]]
           fields]]]

# Usage:
[define-accessors Point  x  y  z]
# Expander generates and registers: macro x, macro y, macro z
# Each is available immediately after the splice:
point: [Point 1.0 2.0 3.0]
px: [x point]   # → [get point "x"] — uses the just-registered macro
```

**Registration timing.** The expander registers each `MacroDecl` from splice output before processing the next entry in the same dict. Macros generated in entry `k` are available for entries `k+1` onward — same semantics as regular macro registration.

**Depth limit applies.** A meta-macro that generates a meta-macro that generates another... is bounded by the total expansion depth limit. Infinite meta-macro chains hit the limit and produce an expansion error.

**Ordering constraint.** A generated macro is only available AFTER the meta-macro call that produced it. A form that appears BEFORE `[define-accessors Point x y z]` in the same dict cannot use `x`, `y`, or `z` as macros. This matches the general rule that macros are available to later forms in the same file.

---

### Explicitly Out of Scope

**`tokens` receive mode** — raw token sequences for embedded DSLs. Not needed: the typed `Expr` system gives structured access to everything the parser produces. If tinct's grammar doesn't handle a construct, the right fix is to extend the grammar, not expose raw tokens to user code. Excluded for complexity and lack of use case.

**Infix operator registration** — requires hooks into the tokenizer, which is Rust-only. Tinct's bracket syntax makes infix operators unnecessary.

**Compile-time type access** — macros run before type-checking and do not see inferred types. Interleaving expansion with type inference (Template Haskell's `reify`) would fundamentally reorganize the pipeline. Excluded.

**Character-level lexer hooks** — excluded because they make programs unreadable: if arbitrary user code can redefine what characters mean, two readers looking at the same file may parse it differently depending on which hooks are loaded. Tinct's security boundary is the capability system (`DirCap`, `NetCap`, `Handle`) — raw character access doesn't bypass it, but it does make programs impossible to reason about statically.

**Expansion order and confluence** — the expander uses top-down, left-to-right expansion order (standard for Racket/Scheme procedural macros). Procedural macros are inherently non-confluent: different expansion orders can produce different results. Top-down left-to-right is the canonical choice. Users relying on expansion-order-dependent behavior write non-portable macros.

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
[macro my-or [let ...args@[Seq Expr]]
  [match [length args]
    [case 0       [quote false]]
    [case 1       [first args]]
    [case [let _]
      [a: [first args]  rest: [rest args]]
      [quote [if [unquote a] [unquote a] [my-or [unquote-splice rest]]]]]]]
```

The zero-arg case returns `[quote false]` — the AST for literal `false`. The one-arg case returns `[first args]` directly — it's already an `Expr` AST node (the macro's return value is `Expr`). The many-arg case produces `[if a a ...]` — `a` appears twice, but in tinct's pure lazy evaluation, a thunk is materialized at most once regardless of reference count, so double-evaluation is semantically safe and carries no performance cost beyond the second lookup.

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
  [tmp: [do-var-node [gensym "tmp"]]]
  # gensym returns String ":tmp:42"; do-var-node wraps it in a VarRef AST node
  [quote [let [[[unquote tmp]: [unquote expr]]] [unquote body]]]]
```

```tinct
[with-tmp [expensive-computation] [+ tmp 1]]
# → [let [[:tmp:42: [expensive-computation]]] [+ :tmp:42 1]]
```

`gensym` returns the String `":tmp:42"`; `do-var-node` wraps it in a `VarRef` AST node. `[unquote tmp]` splices the fresh identifier directly as an identifier wherever it appears: in binding position, in reference position, anywhere. No special splicing primitive needed.

**Edge case — user variable named `tmp`:** Without gensym, the macro would introduce `tmp` and shadow the user's own `tmp`. Gensym produces a node whose name contains `:` (a character in the lexer's denylist that cannot appear in user-written tinct identifiers), so the user's `tmp` is unaffected.

```tinct
[let [tmp: 99]
  [with-tmp [compute] [+ tmp result]]]
# → [let [tmp: 99]
#     [let [[:tmp:42: [compute]]]
#       [+ tmp result]]]   # tmp refers to 99; :tmp:42 is the computed value
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
# Actually: params = Let(...) → flatten-args → returns p.bindings (a [Seq Expr])
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
# Note: a non-empty keyed dict as params (e.g., [fn [x: 1] body]) also hits this arm.
# Keys are stripped; values become params. The type checker then rejects ill-typed params.
# This is not a silent success — it produces a type error, just not a structural one.
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
[macro derive [let ...targets@[Seq Expr]  body@Expr]
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

`splice` is only valid at dict top level. The expander rejects it in expression position — `Splice` is an `Expr` variant, so the type checker cannot distinguish it; the expander must check.

---

### Complex 3: `cond` — Multi-Arm Dispatch with Recursive Expansion

`cond` accepts a sequence of `[test body]` clause pairs plus an optional `[else body]` fallback. It chains them into nested `[if ...]` expressions.

```tinct
[macro cond [let ...clauses@[Seq Expr]]
  [match [length clauses]
    [case 0
      [quote [error "cond: no matching clause"]]]
    [case 1
      [clause: [first clauses]]
      [match [first clause]
        [case [let [name: n] : VarRef]
          [if [= n "else"]
            [quote [unquote [second clause]]]          # [else body] — matches name "else" exactly
            [quote [if [unquote [first clause]]        # [test body] where test happens to be a VarRef
              [unquote [second clause]]
              [error "cond: no matching clause"]]]]]
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

**Edge case — `else` detection:** `[else body]` is detected by extracting the VarRef's `name` field and comparing to the string `"else"` — not by matching any VarRef (which would wrongly treat `[x body]` as an else clause). `else` is imported from stdlib as a constant `true`, making `[else body]` semantically equivalent to `[if true body ...]`.

**Edge case — the fixpoint:** Each expansion step produces one more `[if ...]` and one recursive `[cond ...]`. The total expansion depth limit (100 shared across all macros in the file) bounds this. A `cond` with many clauses may approach the limit; use `[match ...]` directly for very large dispatch tables.

---

### Complex 4: `pragma` — Syntax Classes vs. Manual Validation

With syntax classes, `pragma` is three lines. The expander handles validation; error messages are automatic:

```tinct
[macro pragma [let name@VarRef  value@Literal]
  [quote [pragma [unquote name] [unquote value]]]]
```

```tinct
[pragma optimize true]    # ✓
[pragma [+ 1 2] true]     # expansion error: "'name' expected VarRef, got Call"
[pragma optimize x]       # expansion error: "'value' expected Literal, got VarRef"
```

For custom error messages, declare named syntax classes:

```tinct
[syntax-class pragma-name
  pattern: [let _ : VarRef]
  message: "bare identifier (e.g., optimize, debug)"]

[syntax-class pragma-value
  pattern: [let _ : Literal]
  message: "literal value (e.g., true, 42, \"fast\")"]

[macro pragma [let name@pragma-name  value@pragma-value]
  [quote [pragma [unquote name] [unquote value]]]]
```

```tinct
[pragma [+ 1 2] true]
# "pragma: 'name' — expected bare identifier (e.g., optimize, debug), got Call"
```

**Without syntax classes** (manual validation using `macro-error`), the same macro requires explicit dispatch:

```tinct
[macro pragma [let name@Expr  value@Expr]
  [name-seq: [flatten-args name]]
  [match [length name-seq]
    [case 1
      [match [first name-seq]
        [case [let _ : VarRef]
          [match value
            [case [let _ : Literal]  [quote [pragma [unquote [first name-seq]] [unquote value]]]]
            [case [let _]            [macro-error [span-of value] "pragma value must be a literal"]]]]
        [case [let _]
          [macro-error [span-of [first name-seq]] "pragma name must be a bare identifier"]]]]
    [case [let _]
      [macro-error [span-of name] "pragma: exactly one name allowed"]]]]
```

Both versions produce errors at the exact offending source location. Syntax classes eliminate the dispatch boilerplate while keeping the same error quality.

---

## What Would Change

### Unified-Bindings (`doc/whatif/unified-bindings.md`)

**Current:** The proposal states "parse error" for `[fn [x y] body]` (missing `[let ...]`).
**Proposed:** Corrected to "type error." The parser accepts `[fn any-expr body]`; the type checker enforces `Expr::Let` in param position. This removes hard enforcement from StackFrames for `fn`, `class`, `type` and places it in `check_fn_expr` in `src/typecheck.rs`.
**Impact:** Minor in scope; architectural correctness improvement independent of macros.

### `src/parser.rs` — Pre-Scan and Neutral Key Handling

**Current:** Duplicate detection uses bare-name identity unconditionally. `fn`/`class`/`type` StackFrames enforce `Expr::Let`.
**Proposed:** (1) A pre-scan pass walks the already-parsed AST and collects all `macro` and `syntax-class` declarations, building a registry of form names with their `inject:` defaults. `inject:` is an ordinary dict entry key inside the macro body; the pre-scan extracts it by looking for a top-level `KeyedEntry` with key `"inject"`. **Only bare string-literal `include` paths are followed during pre-scan** — computed-path includes (`[include [str %libdir "/"  v ".llt"]]`) cannot declare macros; the expander raises an error if a computed include produces `macro` or `syntax-class` declarations. (2) Any form with a `macro` declaration in scope gets neutral key handling — the parser does not apply bare-name duplicate detection for that form's argument positions. (3) `fn`/`class`/`type` StackFrames accept any first sub-expression without error (semantic enforcement moved to type checker). (4) `syntax-class` is added to the parser keyword dispatch table with the same `peek_next_horizontal` colon-ahead guard as `fn`, `macro`, `type` — so `[syntax-class: foo]` parses as a dict entry, not a declaration.
**Impact:** Moderate — pre-scan with `inject:` extraction; neutral-key flag per form; StackFrame semantic checks removed; `syntax-class` keyword added.

### `src/expand.rs` — Parse-Stage Transformation Pass

**Current:** Expansion pass handles `defmacro` post-parse, with lazy macro registration during the walk.
**Proposed:** (1) Rename `defmacro` → `macro`. (2) Pre-scan pass evaluates and registers all `Expr::MacroDecl` and `Expr::SyntaxClass` nodes before the main expansion walk. (3) Add splice handling in `expand_document`: when a macro returns `Expr::Splice(forms)`, inject each form. Any `MacroDecl` or `SyntaxClass` in the splice output is **registered immediately** before processing the next splice form — this enables meta-macros (macros generating macros available to subsequent forms). Splice in expression position is an expansion-time error. (4) Validate macro arguments annotated with `@VariantName` or `@syntax-class-name` before calling the macro body; raise `MacroError` on failure. (5) Update argument binding to use `[let ...]` pattern matching. (6) Deep-materialize both input and output. (7) Preserve provenance for runtime errors in macro bodies.
**Impact:** Moderate — pre-scan registration, splice handling, `[let ...]` pattern binding, provenance propagation.

### `src/ast.rs` — New Variants

```rust
Expr::MacroDecl {
    name: String,
    params: Box<Spanned<Expr>>,   // same structure as Fn.params — a LetDecl node
    body: Box<Spanned<Expr>>,
}

Expr::Splice(Vec<Spanned<Expr>>)
```

`Expr::MacroDecl` nodes are filtered from the post-expansion AST (same as `Expr::DefMacro` today) — they must not reach `typecheck.rs` or `eval.rs`. Every new `Expr` variant requires an arm in `eval.rs`, `typecheck.rs`, and `expand.rs`; `MacroDecl` and `Splice` should `panic!` or return an error in eval (expansion guarantees their removal).
**Impact:** Minor — two new variants; exhaustive match arms updated mechanically; eval/typecheck arms for new variants reject unreachable cases.

### `src/typecheck.rs` — Semantic Enforcement for Binding Positions

**Current:** `fn`/`class`/`type` param checking happens in the parser.
**Proposed:** Move to `check_fn_expr`, `check_class_decl`, `check_type_alias`: if the first sub-expression is not `Expr::Let`, emit type error "parameter list must be a `[let ...]` binding declaration." Also enforce: `[unquote val]` inside `[quote ...]` requires `val : Expr`; a macro body's inferred return type must be consistent with `Expr`.
**Impact:** Minor — enforcement moved, plus quote/unquote type checking.

### `src/error.rs` — `ErrorKind::MacroError`

Add `ErrorKind::MacroError { span: Span, message: String }` — distinct from `EvalError::user_error`. A `MacroError` surfaces before type-checking with the span pointing at the offending source location (from `[span-of ...]`). The existing `macro_expansion` provenance field on `EvalError` is used for runtime errors inside macro bodies: when a macro body throws an unexpected runtime error, the expander attaches the call-site span as `macro_expansion` provenance before propagating, so the user sees both where the macro was called and where in the body it failed.
**Impact:** Minor — one new ErrorKind variant; integration with existing `macro_expansion` provenance field.

### `stdlib/prelude.llt` — New Primitives and Breaking Changes

Add: `span-of`, `wrap-in-let`, `let-decl-elems`, `first-or` (sequence helpers).
Add: `macro-error` as a Rust builtin.
Add: `macro-injects` as a Rust builtin — takes a macro name (`Str`), returns the `inject:` default name (`Str`) or `Null` if the macro uses only gensym hygiene.
**`gensym` API:** Zero-arg form returns `":gensym:N"` (String); one-arg form `[gensym "prefix"]` returns `":prefix:N"` (String). The `:` separator is in the lexer's denylist (structurally unforgeable), making the name collision-free. The String return is intentional: callers use `[do-var-node [gensym "prefix"]]` (or equivalent) to construct a VarRef AST node from the string when an identifier is needed. This avoids making gensym AST-aware. All call sites pass the gensym result to `do-var-node` or equivalent.
**`macro` keyword:** `macro` becomes a reserved keyword. The existing 27 corpus test files in `tests/corpus/eval/macros/` using `defmacro` migrate to `macro`.

### `src/ast.rs` — `Expr::SyntaxClass`

```rust
Expr::SyntaxClass {
    name: String,
    pattern: Box<Spanned<Expr>>,   // a [let ...] binding pattern (same as case arm patterns)
    message: String,               // user-facing description for validation failures
}
```

`Expr::SyntaxClass` is a declaration form — same treatment as `Expr::MacroDecl`. Both are filtered from the post-expansion AST before type-checking; both have `panic!` arms in the evaluator (expansion guarantees their removal). Syntax class declarations are pre-scanned and registered alongside `MacroDecl` nodes before the main expansion walk.

When a macro parameter is annotated with a syntax class name (`name@pragma-name`) or a built-in `Expr` variant (`name@VarRef`), the expander validates the argument before calling the macro body:
- Built-in variant: "argument 'name' expected VarRef, got Call"
- Named syntax class: uses the `message:` field
Validation failure raises `MacroError` at the call-site span.

### `stdlib/ast.llt` (new file)

Defines `Entry`, `Annotation`, and `Expr` nominal types plus `flatten-args`, `ident`. This file must be explicitly imported by macro-writing code: `[include %libdir "ast.llt"]`. Macro bodies that use `Expr` variant names (`Let`, `VarRef`, `Call`, etc.) without importing this file will get undefined-variable errors.

`[ident str@Str] -> VarRef` — constructs `VarRef(name: str)` from any string. For reconstructing existing identifiers from extracted name fields; not for generating fresh names (use `gensym` for that).
**Impact:** New file, ~70 lines.

### `ast_to_dict` — Typed Expr Variant Migration

**Current:** `ast_to_dict` produces plain dicts with string `type:` fields: `{type: "var-ref" name: "x"}`.
**Proposed:** Produces typed `Expr` variant values: `VarRef(name: "x")`, `Call(func: ..., args: [...])`, etc.
**Impact: Major** — every existing AST consumer must migrate from string-type dispatch to variant pattern matching:
- `stdlib/macros.llt` — `tmpl`, `do`, `begin` macros use `[get "type" node]` string equality (~15 node-building sites)
- `stdlib/formatter/compact.llt` and `stdlib/formatter/pretty.llt` — both dispatch on `{type: "..."}` string fields with wildcard fallbacks; all arms need migration; new arms needed for `Splice` and `MacroDecl`
- `src/builtins_meta.rs` — `ast-of` constructs output in the old schema

### `stdlib/syntax.llt` (new file)

`macro` declarations for fn/class/type let-softening, using `flatten-args` from `stdlib/ast.llt`. Loading mechanism: the expander's pre-scan must execute `include %libdir "syntax.llt"` at file-load time (before parsing user code), not at runtime. This requires the pre-scan to resolve includes and evaluate their macro declarations. When a user's file starts with `[include %libdir "syntax.llt"]`, that include fires the pre-scan registration of the fn/class/type macros before any other form in the file is expanded.
**Impact:** New file, ~30 lines; include-at-pre-scan mechanism is the non-trivial part.

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
- Dybvig, R.K., Hieb, R. & Bruggeman, C. (1993). "Syntactic Abstraction in Scheme." *Lisp and Symbolic Computation*, 5(4), 295–326. — [`syntax-case`: procedural macros with automatic hygiene via syntax objects; expansion termination bounds; non-confluence of procedural macros]
- Culpepper, R. & Felleisen, M. (2010). "Fortifying Macros." *Journal of Functional Programming*, 20(5-6), 517–549. — [syntax classes as the declarative validation layer over procedural macros; the ergonomic gap that tinct's `[match [length args] ...]` fills procedurally]
- Krishnamurthi, S. (2001). "Linguistic Reuse." Ph.D. thesis, Rice University. — [syntactic abstraction and parse-time hooks as the mechanism for language extensibility from user code]
