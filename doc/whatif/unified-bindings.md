# What If: Unified Binding Declarations via `[let ...]` for tinct

**State:** Proposal

What would it take to give every binding declaration in tinct a single self-announcing form — so that any bracket containing binding declarations starts with `let`, and any bracket without `let` is always an expression?

## Current State

Tinct binds names in several contexts, each with its own syntactic convention:

```tinct
[fn  [x@Int y@Float]  body]           # param bracket — context makes it a binding list
[class  [a b c]  methods...]          # TypeVar bracket — context makes it a name list
[type  [a b]  [or a b]]               # alias param bracket — context makes it a name list
fn@[bind: [a b c]  return: c  ...]    # bind: value — treated as a name list
```

The same bracket form `[a b c]` means different things in different positions:

- In `[fn [a b c] body]` — three parameter names
- In `[class [a b c] ...]` — three TypeVar declarations
- In a value expression context — implied call `a(b, c)`

A reader unfamiliar with tinct sees `[fn [a b c] body]` and can reasonably parse `[a b c]` as calling function `a` with args `b` and `c` from the enclosing scope. The actual meaning — three parameter declarations — is only knowable from the special-case rule that `fn`'s first bracket is a binding list.

### The invariant that does not hold

Tinct has one general parsing rule: `[head arg arg ...]` is an implied call when `head` is an identifier in call position. Binding brackets **violate this rule silently** — they look like implied calls but are not parsed as such. This requires the parser to carry implicit knowledge about which keywords put their first bracket in "binding mode."

### What's Missing

1. A self-announcing binding form that is unambiguous without enclosing context
2. A consistent invariant: any bracket not starting with `let` is always an expression
3. Elimination of the per-keyword special cases that put brackets into "binding mode"

## Why Unified Bindings Matter for tinct

**One parsing rule.** Today: `[a b c]` is an implied call, *except* when it appears as the first bracket after `fn`, `class`, or `type`. With `[let ...]`: `[a b c]` is always an implied call. No exceptions. The rule is complete.

**Readable without foreknowledge.** `[fn [let x@Int y@Float] body]` is unambiguous to a reader who has never seen tinct: the `let` keyword signals binding declaration. `[fn [x@Int y@Float] body]` is not — it looks like calling `x@Int` as a function with arg `y@Float`.

**Self-describing code.** Every binding declaration carries its own announcement. No context is needed to know that `[let a b c]` introduces names `a`, `b`, `c`.

**Uniform refactoring.** Because `[let ...]` is always a binding list and other brackets are always expressions, tools can rename bindings, extract functions, and analyze scope without knowing which enclosing keyword they're inside.

## Design

### The `[let ...]` Form

`let` is a keyword. `[let ...]` is a **binding declaration list** — always. Every element is one of:

- `name` — bare lowercase identifier, untyped binding
- `name@Type` — typed binding: name constrained to Type
- `_` — wildcard, matches anything, introduces no binding

```tinct
[let a]                    # one untyped binding
[let a b c]                # three untyped bindings
[let x@Int y@Float]        # two typed bindings
[let a b@[Seq elem] _]     # mixed: untyped, typed with composite type, wildcard
```

`[let ...]` **never parses as an implied call**. The bracket containing `[let ...]` is always a binding list, regardless of context.

Any bracket NOT starting with `let` is always an expression. This invariant is unconditional.

### Function Parameters

`fn`'s first bracket must be `[let ...]`. There is no other valid form:

```tinct
[fn [let x@Int y@Float] [+ x y]]

[fn [let xs@Seq@a  f@[Fn@b [a]]]
  [map f xs]]

# Zero params — empty [let]
[fn [let] 42]

# Variadic
[fn [let x@Int  ...rest@Seq@Int]
  [+ x [sum rest]]]
```

`[fn [x@Int y@Float] body]` is a **parse error**: the parser expects `[let ...]` as the first bracketed expression inside `fn`.

### Class TypeVar Declarations

```tinct
Addable: [class [let a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [let a b]]]

Equatable: [class [let a]
  eq?: [fn@Bool [let a a]]]

Functor: [class [let f]  [kinds: [f: Operator]]
  fmap: [fn@[return: [f b]] [let g@[Fn@b [a]]  xs@[f a]]]]
```

`[class [a b c] ...]` is a parse error.

Note that method signatures within a class body use `[fn@T [let params]]` — the fn inside a class body follows the same rule as any other fn.

### Type Alias Parameters

```tinct
Either:   [type [let a b] [or a b]]
Pair:     [type [let a b] [record first: a  second: b]]
Nullable: [type [let a]   [or a Null]]

# No-param alias — no [let ...], just the body type expression
NullableInt: [type [or Int Null]]
```

`[type [a b] body]` is a parse error. No-parameter aliases have no leading bracket (just the type expression), so there is no binding bracket to annotate.

### `bind:` in `fn@[...]`

The `bind:` key stays as the explicit metadata key in `fn@[...]`. Its value uses `[let ...]`:

```tinct
scale: [fn@[bind: [let a b c]  return: c  constraint: [a: Numeric  b: Numeric  [$Multipliable a b c]]]
  [let x@a  factor@b]
  [* x factor]]

min: [fn@[bind: [let a]  return: a  constraint: [a: Comparable]  doc: "Return smallest element"]
  [let xs@Seq@a] ...]
```

`bind: [a b c]` is a parse error: the value of `bind:` must be a `[let ...]` form.

`[let ...]` does not appear bare in `fn@[...]` metadata dicts — only as the value of `bind:`. Mixing a positional `[let ...]` with keyed `return:`, `constraint:`, `doc:` entries would make the scope of the binding declaration ambiguous:

```tinct
# WRONG: are constraint: and doc: inside the let scope?
fn@[let a b c  constraint: [a: Comparable]  doc: "text"]

# RIGHT: bind: separates the binding declaration from the keyed metadata
fn@[bind: [let a b c]  constraint: [a: Comparable]  doc: "text"]
```

### Instance Arm Keys

All type patterns in instance arms are binding lists. There are no constructor patterns at the type level — only type constants, type applications, TypeVars, and wildcards. Instance arms use `[let ...]` directly as the arm key:

```tinct
[instance Addable
  [let a@Int   b@Int   c@Int  ]: [+: [fn@Int   [let x@Int   y@Int  ] [builtin-add x y]]]
  [let a@Int   b@Float c@Float]: [+: [fn@Float [let x@Int   y@Float] [builtin-add x y]]]
  [let a@Float b@Float c@Float]: [+: [fn@Float [let x@Float y@Float] [builtin-add x y]]]]

[instance Appendable
  [let a@Str]:        [concat: [fn@Str [let x@Str y@Str] [builtin-str-concat x y]]
                       empty:  [fn@Str [let] ""]]
  [let a@[Seq elem]]: [concat: [fn@a [let xs@a ys@a] [builtin-seq-concat xs ys]]
                       empty:  [fn@a [let] []]]
  [let a@[Map k v]]:  [concat: [fn@a [let m1@a m2@a] [merge m1 m2]]
                       empty:  [fn@a [let] []]]]

[instance Functor
  [let f@Seq]:   [fmap: [fn@[return: [Seq b]] [let g@[Fn@b [a]] xs@[Seq a]] [map g xs]]]
  [let f@Maybe]: [fmap: [fn@[return: [Maybe b]] [let g@[Fn@b [a]] m@[Maybe a]]
                         [match m  [Some v]: [Some [g v]]  None: None]]]]
```

The `[pattern [...]]` form is retired entirely — `[let ...]` is its replacement.

### Match Arms

Match arm keys with constructor patterns are a **distinct syntactic category** from binding lists. A binding list (`[let ...]`) scopes its bindings to whatever body or value follows the position it occupies. A structural pattern (`[Ok v]`) scopes its bindings to the arm body — a scoping rule that belongs to the pattern system, not to the expression system.

`[Ok [let v]]: body` does not work: `[let v]` is nested inside the `Ok` expression. In expression semantics, `v` would scope to `[Ok ...]` itself — not to `body` across the `:`. There is no natural "what follows" for the inner `[let ...]`.

The invariant therefore covers **binding list positions** only. Match arm keys with constructor patterns remain in pattern context — a distinct category where the pattern system determines scoping, not the expression/binding-list system.

**Binding-only match arm keys** use `[let ...]` — here the scoping is natural because `[let ...]` occupies the entire arm key position, and its bindings scope to the value after `:`:

```tinct
[match x
  [let n@Int]:   [+ n 1]    # [let ...] is the entire key; n scopes to body ✓
  [let _]:       0]         # wildcard
```

**Constructor patterns** remain as they are — `[Ok v]` in pattern position, `v` introduced as a new binding scoped to the arm body by the pattern system:

```tinct
[match result
  [Ok v]:  v                # pattern context: v is a new binding, scopes to body
  [Err e]: [log e]
  _:       0]
```

The two forms coexist cleanly: `[let ...]` for pure binding arms (no structural test), existing constructor patterns for structural arms. Both are arm keys used before `:`.

**What this means for the invariant:** The rule "any bracket used as a binding list must start with `[let ...]`" holds. Constructor patterns are **not binding lists** — they are structural patterns. The parser distinguishes them: `[let ...]` (binding list) vs `[Constructor ...]` (structural pattern, where Constructor is an uppercase name). No ambiguity arises because lowercase-only brackets used as arm keys require `[let ...]`, while uppercase-head brackets are structural patterns.

### What `[let ...]` Does Not Do

`[let ...]` is a binding DECLARATION — it introduces names into a scope. It is not:

- A let-expression with values (there is no `[let x 5  x + 1]` form — dict entries `x: 5` handle that)
- A structural pattern (constructor patterns `[Ok v]` have pattern-specific scoping that cannot be expressed as a binding list — `v` must scope to the arm body across the `:`, which the `[let ...]` expression model cannot express when nested inside another expression)
- A type-level expression (type dicts and type-stage expressions are distinct)

Structural patterns in match arms are a different syntactic category from binding lists, with their own scoping rules managed by the pattern system. The `[let ...]` reform does not touch this category.

### Parsing Invariant

After this change, the parser has one complete rule for brackets:

> A bracket starting with `let` is always a binding declaration list (`Expr::LetDecl`). Every other bracket is always an expression — evaluated as an implied call if its first element is an identifier in call position, or as a positional/keyed dict otherwise.

No context-specific exceptions. No "binding mode" that activates for certain keywords.

## What Would Change

### `src/lexer.rs` — `Token::Let` keyword

**Current:** No `let` keyword. `let` would tokenize as `Token::Identifier("let")`.  
**Proposed:** Add `let` to the keyword table. `Token::Let`. The identifier `let` is no longer available as a variable name.  
**Impact:** Minor — one new token.

### `src/ast.rs` — `Expr::LetDecl`

**Current:** No `Expr::LetDecl` variant.  
**Proposed:**
```rust
Expr::LetDecl {
    bindings: Vec<Spanned<Expr>>,  // each: VarRef (bare name), Annotated (name@Type), or Wildcard (_)
}
```
**Impact:** Minor — one new AST variant; all exhaustive match arms gain a `LetDecl` arm (mechanical).

### `src/parser.rs` — `StackFrame::LetDecl` and frame updates

**Current:** fn/class/type frames apply context-specific "binding mode" to their first bracket.  
**Proposed:** New `StackFrame::LetDecl` collects binding entries and closes to `Expr::LetDecl`. The `[let ...]` form is parsed using the existing fn-param-list path. Enclosing frames (`Fn`, `ClassDecl`, `TypeAlias`, `InstanceDecl`, `Match`) receive `Expr::LetDecl` via `push_expr_to_parent` and treat it as their binding list.

Per-keyword changes:
- `StackFrame::Fn`: first expression must be `Expr::LetDecl`; parse error otherwise
- `StackFrame::ClassDecl`: first expression must be `Expr::LetDecl` (the TypeVar list)
- `StackFrame::TypeAlias`: if first expression is `Expr::LetDecl`, it is the param list; otherwise it is the body (no-param alias)
- `StackFrame::InstanceDecl`: `Expr::LetDecl` followed by `:` = arm key
- `StackFrame::Match`: `Expr::LetDecl` followed by `:` = binding arm key

**Impact:** Moderate — removes three context-specific binding-mode bracket handlers; adds one new `StackFrame::LetDecl`; updates five enclosing frames.

### `src/typecheck.rs` — binding extraction from `Expr::LetDecl`

**Current:** fn/class/type typecheck handlers extract bindings from raw bracket expressions using context-specific logic.  
**Proposed:** Each context receives `Expr::LetDecl` and extracts bindings from `bindings: Vec<Spanned<Expr>>`. The semantic interpretation (value params vs TypeVars vs type-pattern bindings) is context-determined, but the extraction mechanics are shared.  
**Impact:** Moderate — binding extraction logic centralizes; each handler gains a `LetDecl` arm with semantically equivalent logic.

### `src/formatter.rs` — `Expr::LetDecl` formatting

**Current:** No formatter arm.  
**Proposed:** Format `Expr::LetDecl` as `[let b1  b2  b3]`, matching the width-measurement and inline/multi-line logic of the fn param list formatter.  
**Impact:** Minor — one new formatter arm.

### `stdlib/prelude.llt` — binding syntax migration

**Current:** All fn/class/type/instance declarations use implicit binding brackets.  
**Proposed:** Migrate every binding bracket to `[let ...]`. Purely mechanical; no semantic changes.  
**Impact:** Major in scope (every declaration in the stdlib), minor in complexity (textual substitution).

### Corpus tests — binding syntax migration

**Current:** All test files use implicit binding brackets.  
**Proposed:** Migrate all `[fn [params]]`, `[class [tvars]]`, `[type [params]]` to `[let ...]` form. Mechanical.  
**Impact:** Moderate in scope; minor in complexity.

## Prerequisites

None — this is a self-contained parser and AST change with no dependencies on other sprints. The semantic behavior of all binding forms is preserved; only the syntax is regularized.

## References

- Milner, R. (1978). "A Theory of Type Polymorphism in Programming." *Journal of Computer and System Sciences*, 17(3), 348–375. — [let-binding as the canonical polymorphic binding form in ML; the historical source of `let` as a binding keyword]
- Landin, P.J. (1966). "The Next 700 Programming Languages." *Communications of the ACM*, 9(3), 157–166. — [ISWIM's `where`-clauses and let-forms as syntactically distinct from application; the origin of the principle that binding and application should look different]
