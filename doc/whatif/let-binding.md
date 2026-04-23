# What If: `let` Binding Form for tinct

What would it take to enable non-recursive local bindings everywhere
in tinct — including inside function bodies?

## Current State

tinct has two scoping mechanisms, both using the same parent-chain
lookup (DESIGN.md §Scope Chains):

1. **Within a dict (letrec):** All entries in a single `[...]` share one
   environment. Entries can reference each other regardless of order,
   including mutual recursion.

   ```lisp
   [
     x: 10
     y: [call $+ $x 1]    # sees x (same dict, letrec)
     z: [call $+ $y 1]    # sees y and x
   ]
   ```

2. **Between sequential expressions:** Each expression's result dict
   becomes the parent scope for the next expression. Names from earlier
   expressions are visible but can be shadowed.

   ```lisp
   [x: 10]
   [y: [call $+ $x 1]]    # sees x from parent scope
   ```

Sequential expressions already provide `let*` semantics at the
document level. But inside function bodies, only a single expression
is allowed:

```lisp
# One expression — no intermediate bindings
double: [fn [x] [call $* $x 2]]

# Workaround for intermediate bindings: wrapper dict + dot access
process: [fn [data]
    [
      cleaned: [call $clean $data]
      result: [call $transform $cleaned]
    ].result]    # extract the final value
```

The wrapper-dict pattern works but is verbose and unintuitive for
multi-step computations.

## What `let` Binding Would Provide

1. **Intermediate bindings in functions.** Multi-step function bodies
   without wrapper dicts:
   ```lisp
   process: [fn [data]
       [cleaned: [call $clean $data]]
       [call $transform $cleaned]]
   ```

2. **Consistency.** Sequential expressions work at document level —
   they should work inside function bodies too. Same scoping rules,
   same mental model.

3. **Pattern matching integration.** When pattern matching
   (`doc/whatif/pattern-matching.md`) is adopted, match arm bodies
   will need intermediate bindings. Sequential expressions in
   function bodies enable this naturally.

## Approaches

### Approach B: Sequential Expressions in Function Bodies

Extend function bodies to accept expression sequences, reusing the
existing sequential scoping mechanism:

```lisp
process: [fn [data]
    [cleaned: [call $clean $data]]     # first expression
    [validated: [call $validate $cleaned]]  # second expression
    [call $transform $validated]]      # final expression (return value)
```

Each expression in the body is a sequential scope step — same as
document-level sequential expressions. The last expression's value
is the function's return value.

**Semantics:** A function body `[fn [params] e₁ e₂ ... eₙ]` evaluates
as if the expressions were sequential document expressions: each `eᵢ`'s
result dict becomes the parent scope for `eᵢ₊₁`. The value of the
function is the value of `eₙ`.

**Interaction with `[fn [params] body]` parsing:** The parser currently
treats everything after the params list as a single body expression.
Sequential bodies require the parser to recognize multiple expressions
in function position.

**Delimiter:** Function body expressions are separated by the same
mechanism as document-level expressions — either newlines or explicit
separation. Since `[fn [x] ...]` is a bracket-delimited form, the
closing `]` unambiguously terminates the sequence.

```lisp
# Single-expression body (unchanged)
[fn [x] [call $* $x 2]]

# Multi-expression body
[fn [data]
    [cleaned: [call $clean $data]]
    [call $transform $cleaned]]
```

**Pros:**
- No new keyword or special form
- Reuses existing sequential scoping mechanism
- Consistent with document-level behavior
- More general than `let` — any number of intermediate steps
- Works naturally inside match arms and other compound forms

**Cons:**
- Grammar change: function body becomes an expression sequence
- Parser must determine where body expressions are separated
  (whitespace-sensitive or explicit delimiter)

## Recommendation

**Approach B: Sequential expressions in function bodies.**

### Rationale

1. **Consistency over novelty.** Sequential expression scoping already
   exists at the document level and works well. Extending it to function
   bodies is the same mechanism applied to a new position — no new
   concept for users to learn.

2. **No new keywords.** Unlike `let`/`let*`, Approach B adds no new
   grammar keywords. The parser change is localized to function body
   parsing.

3. **More general than `let`.** Sequential bodies support any number
   of intermediate steps, not just a binding-then-body pattern. This
   is more tinct-idiomatic — each step can be a full expression, not
   just a binding.

4. **Pattern matching readiness.** When `[match]`
   (`doc/whatif/pattern-matching.md`) is adopted, match arm bodies will
   need intermediate bindings. Sequential function bodies enable this:
   ```lisp
   [match $val
     [ok: $v]
       [cleaned: [call $clean $v]]
       $cleaned
     [err: $msg]
       [call $log $msg]
       [call $error $msg]]
   ```

5. **Nix parallel.** Nix's `let ... in` provides sequential bindings
   as a separate form. tinct's sequential expressions already serve
   this role at the document level. Extending to function bodies
   completes the coverage without adding Nix-style `let`.

### Phased Adoption

#### Phase 1: Multi-Expression Function Bodies

Extend the parser to accept multiple expressions in function body
position. Each expression's result dict becomes the parent scope for
the next. The last expression's value is the return value.

Implementation:
- Grammar change: `fn_body` rule accepts expression sequence
- Parser produces `Expr::Fn` with `body: Vec<Spanned<Expr>>` instead
  of `body: Spanned<Expr>` (or desugar to nested sequential evaluation)
- Evaluator: evaluate body expressions sequentially with chained scopes
- Type checker: infer each expression in sequence, threading the
  environment

#### Phase 2: Sequential Bodies in Match Arms

When pattern matching is adopted, match arm bodies naturally support
the same sequential expression mechanism.

### Prerequisites

- Parser change to function body parsing.
- The evaluator's sequential expression handling (already implemented
  for documents) must be factored out for reuse in function bodies.

### Trigger

Adopt when:
- Function bodies frequently need intermediate bindings (already a
  common pattern in formatters and transforms)
- Pattern matching adoption requires multi-expression match arm bodies
- The wrapper-dict pattern (`[... result: expr].result`) becomes a
  frequent ergonomic complaint

## References

- Haskell Report §3.12: Let expressions and where clauses. Both are
  letrec — bindings can reference each other.
- Scheme R7RS §4.2.2: `let`, `let*`, `letrec`, `letrec*`. Four binding
  forms with different scoping rules.
- Launchbury, J. (1993). "A natural semantics for lazy evaluation." In
  *POPL '93*, pp. 144–154. ACM. — Formal semantics for letrec in lazy
  languages.
- Nakata, K. & Hasegawa, M. (2009). "Small-step and big-step semantics
  for call-by-need." *Journal of Functional Programming*, 19(6), 699–722.
  — Cycle detection semantics for letrec.
