# `let` Binding Form

## Overview

Sequential `let`-style bindings inside `[fn ...]` bodies give functions a
natural way to name intermediate values without nested wrapper constructs.
The same sequential scoping mechanism that works at document level works
inside function bodies and `[match]` arm bodies. No new keyword is
introduced — this is the existing sequential scoping model applied to
function body position.

## Design

### Multi-Expression Function Bodies

Function bodies accept expression sequences, reusing the existing sequential
scoping mechanism. No new keyword or special form is introduced — this is the
same mechanism that already exists at document level, applied to function body
position. It enables sequential scoping in fn bodies and `[match]` arm bodies.

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

The parser treats everything after the params list as a body sequence.
Function body expressions are separated by the same mechanism as
document-level expressions — either newlines or explicit separation.
Since `[fn [x] ...]` is a bracket-delimited form, the closing `]`
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

4. **Pattern matching readiness.** `[match]` arm bodies use
   intermediate bindings via the same sequential function body
   mechanism:

   ```tinct
   [match val
     [ok: v]:
       [cleaned: [clean v]]
       cleaned
     [err: msg]:
       [log msg]
       [error msg]]
   ```

5. **Nix parallel.** Nix's `let ... in` provides sequential bindings
   as a separate form. tinct's sequential expressions already serve
   this role at the document level. Extending to function bodies
   completes the coverage without adding Nix-style `let`.

## Implementation

### Parser (`parser.rs`)

The `fn` form parser collects multiple expressions after the params
list until the closing `]`. `[fn [x] e1 e2]` desugars to nested
sequential evaluation at parse time.

Backward compatible: single-expression bodies are a sequence of
length 1.

### AST (`parser.rs`)

The parser desugars multi-expression bodies to nested `Expr::Sequential`
nodes at parse time, reusing existing evaluator and type checker paths
without changes to the AST representation.

### Evaluator (`eval.rs`)

With desugaring at parse time, no evaluator change is needed — the
existing `eval_sequential_expressions` handles the nested structure.

### Type Checker (`typecheck.rs`)

With desugaring at parse time, no change is needed — `infer_sequential`
handles the threaded environment.

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
