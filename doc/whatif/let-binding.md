# What If: `let` Binding Form for tinct

What would it take to enable non-recursive local bindings everywhere
in tinct — including inside function bodies?

## Current State

tinct has two scoping mechanisms, both using the same parent-chain
lookup (doc/09-documents.md §Scope Chains):

1. **Within a dict (letrec):** All entries in a single `[...]` share one
   environment. Entries can reference each other regardless of order,
   including mutual recursion.

   ```tinct
   [
     x: 10
     y: [+ x 1]    # sees x (same dict, letrec)
     z: [+ y 1]    # sees y and x
   ]
   ```

2. **Between sequential expressions:** Each expression's result dict
   becomes the parent scope for the next expression. Names from earlier
   expressions are visible but can be shadowed.

   ```tinct
   [x: 10]
   [y: [+ x 1]]    # sees x from parent scope
   ```

Sequential expressions already provide `let*` semantics at the
document level. But inside function bodies, only a single expression
is allowed:

```tinct
# One expression — no intermediate bindings
double: [fn [x] [* x 2]]

# Workaround for intermediate bindings: nested call fn
# Note: dot access on a bracket expr ([{...}].result) is not valid tinct.
# Access chains must start from a var_ref. The only single-expression
# workaround is a nested function application that names each intermediate:
process: [fn [data]
    [call [fn [cleaned] [transform cleaned]]
          [clean data]]]
```

The nested-fn workaround works but is verbose and unintuitive for
multi-step computations — each additional intermediate requires another
nested `[call [fn [name] ...] value]` layer.

### What's Missing

1. **Multi-expression function bodies** — functions are limited to a
   single body expression, forcing wrapper dicts for intermediate
   bindings.
2. **Consistent scoping model** — sequential expressions work at
   document level but not inside function bodies, creating an
   asymmetry in the language.
3. **Match arm bodies** — when pattern matching is adopted, match
   arms will need the same intermediate binding capability.

## What `let` Binding Would Provide

1. **Intermediate bindings in functions.** Multi-step function bodies
   without wrapper dicts:
   ```tinct
   process: [fn [data]
       [cleaned: [clean data]]
       [transform cleaned]]
   ```

2. **Consistency.** Sequential expressions work at document level —
   they should work inside function bodies too. Same scoping rules,
   same mental model.

3. **Pattern matching integration.** When pattern matching
   (`doc/whatif/pattern-matching.md`) is adopted, match arm bodies
   will need intermediate bindings. Sequential expressions in
   function bodies enable this naturally.

## Design

Extend function bodies to accept expression sequences, reusing the
existing sequential scoping mechanism. No new keyword or special form
is introduced — this is the same mechanism that already exists at
document level, applied to function body position.

### Syntax

```tinct
# Single-expression body (unchanged)
[fn [x] [* x 2]]

# Multi-expression body
[fn [data]
    [cleaned: [clean data]]     # first expression
    [validated: [validate cleaned]]  # second expression
    [transform validated]]      # final expression (return value)
```

Each expression in the body is a sequential scope step. The last
expression's value is the function's return value.

### Semantics

A function body `[fn [params] e1 e2 ... en]` evaluates as if the
expressions were sequential document expressions: each `ei`'s result
dict becomes the parent scope for `ei+1`. The value of the function
is the value of `en`.

This is `let*` semantics (sequential, non-recursive bindings), as
opposed to the `letrec` semantics of entries within a single `[...]`
dict. The distinction matters for lazy evaluation: in a letrec dict,
all bindings share one environment and can reference each other; in
a sequential body, each step's bindings are only visible to
subsequent steps.

Formally, the desugaring is:

```
[fn [params] e1 e2 ... en]
  ==>
[fn [params] (let* e1 (let* e2 (... en)))]
```

where `let* ei body` evaluates `ei`, extends the environment with
any bindings `ei` produces (if `ei` is a dict), and evaluates `body`
in the extended environment. This corresponds to the existing
`eval_sequential_expressions` mechanism in the evaluator.

### Interaction with Lazy Evaluation

Each intermediate binding expression is a thunk: `[cleaned: [clean data]]` creates a thunk for `cleaned` that is forced only
when `cleaned` is referenced in a subsequent expression. This
preserves tinct's call-by-need semantics — intermediate bindings
that are never used are never evaluated.

The key invariant: the sequential scope chain must not break sharing.
If two subsequent expressions reference the same intermediate binding,
they must share the same thunk (not create independent copies). The
existing document-level sequential mechanism already maintains this
invariant.

### Interaction with Parsing

The parser currently treats everything after the params list as a
single body expression. Sequential bodies require the parser to
recognize multiple expressions in function position. Function body
expressions are separated by the same mechanism as document-level
expressions — either newlines or explicit separation. Since
`[fn [x] ...]` is a bracket-delimited form, the closing `]`
unambiguously terminates the sequence.

### Interaction with Type Inference

Each expression in the sequence is inferred independently, with the
environment threaded forward. The function's return type is the type
of the final expression `en`. Intermediate dict expressions contribute
their field types to the environment for subsequent expressions,
matching the existing sequential inference in `infer_sequential`.

### Rationale

1. **Consistency over novelty.** Sequential expression scoping already
   exists at the document level and works well. Extending it to
   function bodies is the same mechanism applied to a new position —
   no new concept for users to learn.

2. **No new keywords.** Unlike `let`/`let*`, this design adds no new
   grammar keywords. The parser change is localized to function body
   parsing.

3. **More general than `let`.** Sequential bodies support any number
   of intermediate steps, not just a binding-then-body pattern. This
   is more tinct-idiomatic — each step can be a full expression, not
   just a binding.

4. **Pattern matching readiness.** When `[match]`
   (`doc/whatif/pattern-matching.md`) is adopted, match arm bodies will
   need intermediate bindings. Sequential function bodies enable this:
   ```tinct
   [match val
     [ok: v]
       [cleaned: [clean v]]
       cleaned
     [err: msg]
       [log msg]
       [error msg]]
   ```

5. **Nix parallel.** Nix's `let ... in` provides sequential bindings
   as a separate form. tinct's sequential expressions already serve
   this role at the document level. Extending to function bodies
   completes the coverage without adding Nix-style `let`.

## What Would Change

### Parser (`parser.rs`)

**Current:** The `fn` form parser treats everything after the params
list as a single body expression. `[fn [x] e]` produces
`Expr::Fn { params, body: e }`.

**Proposed:** The `fn` form parser collects multiple expressions after
the params list until the closing `]`. `[fn [x] e1 e2]` produces
`Expr::Fn { params, body: vec![e1, e2] }`, or desugars to nested
sequential evaluation at parse time.

**Impact:** Moderate — localized grammar change to `fn_body` rule.
Backward compatible: single-expression bodies are a sequence of
length 1.

### AST (`parser.rs`)

**Current:** `Expr::Fn { params, body: Box<Spanned<Expr>> }` — a
single body expression.

**Proposed:** Either `body: Vec<Spanned<Expr>>` (direct
representation) or desugar to nested `Expr::Sequential` nodes at
parse time. Desugaring is simpler — it reuses existing evaluator
and type checker paths without changes.

**Impact:** Minor if desugared at parse time; Moderate if the AST
representation changes.

### Evaluator (`eval.rs`)

**Current:** Function application evaluates a single body expression
in the closure's environment extended with parameter bindings.

**Proposed:** If desugared at parse time, no evaluator change is
needed — the existing `eval_sequential_expressions` handles the
nested structure. If the AST uses `Vec<Expr>`, the function
application path must call `eval_sequential_expressions` on the
body list.

**Impact:** Minor (desugaring) to Moderate (direct representation).

### Type Checker (`typecheck.rs`)

**Current:** `infer_fn` infers the body as a single expression.

**Proposed:** If desugared, no change — `infer_sequential` handles
it. If direct, `infer_fn` must thread the environment through
multiple body expressions and use the final expression's type as
the return type.

**Impact:** Minor (desugaring) to Moderate (direct representation).

## Phased Adoption

### Phase 1: Multi-Expression Function Bodies

Extend the parser to accept multiple expressions in function body
position. Each expression's result dict becomes the parent scope for
the next. The last expression's value is the return value.

Implementation:
- Grammar change: `fn_body` rule accepts expression sequence
- Parser desugars to nested `Expr::Sequential` (preferred) or
  changes `Expr::Fn` body to `Vec<Spanned<Expr>>`
- Evaluator: if desugared, no change needed; otherwise evaluate
  body expressions sequentially with chained scopes
- Type checker: if desugared, no change needed; otherwise infer
  each expression in sequence, threading the environment

### Phase 2: Sequential Bodies in Match Arms

When pattern matching is adopted, match arm bodies naturally support
the same sequential expression mechanism. The desugaring approach
makes this trivial — match arm bodies desugar to the same nested
`Expr::Sequential` form.

### Prerequisites

- Parser change to function body parsing (Phase 1 only dependency).
- The evaluator's sequential expression handling (already implemented
  for documents) must be factored out for reuse in function bodies if
  not using the desugaring approach.
- No dependency on other whatif features or TODO.md sprints.

### Trigger

- When function bodies frequently need intermediate bindings (already
  a common pattern in formatters and transforms)
- When pattern matching adoption requires multi-expression match arm
  bodies
- When the wrapper-dict pattern (`[... result: expr].result`) becomes
  a frequent ergonomic complaint

## References

- Launchbury, J. (1993). "A natural semantics for lazy evaluation." In
  *POPL '93*, pp. 144--154. ACM. — Formal semantics for `letrec` in lazy
  languages. Defines the thunk-update model that sequential function
  bodies must preserve: each intermediate binding is a thunk, forced at
  most once.
- Ariola, Z.M. & Felleisen, M. (1997). "The call-by-need lambda
  calculus." *Journal of Functional Programming*, 7(3), 265--301. —
  Formal distinction between `let` (non-recursive, sequential) and
  `letrec` (recursive, shared environment) in call-by-need. Sequential
  function bodies correspond to nested `let` in this calculus.
- Nakata, K. & Hasegawa, M. (2009). "Small-step and big-step semantics
  for call-by-need." *Journal of Functional Programming*, 19(6),
  699--722. — Cycle detection semantics for letrec, relevant to ensuring
  sequential bindings don't accidentally introduce cycles.
- Haskell Report §3.12. Let expressions and where clauses. — Both are
  `letrec` — bindings can reference each other. tinct's sequential
  bodies provide `let*` semantics instead.
- Scheme R7RS §4.2.2. `let`, `let*`, `letrec`, `letrec*`. — Four
  binding forms with different scoping rules. tinct's sequential bodies
  correspond to `let*`.
