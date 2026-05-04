# What If: Type Predicates for tinct

What would it take to add type predicate builtins that distinguish
values by their runtime type?

## Current State

tinct has one type predicate builtin:

- **`seq?`** — returns `true` if the value is a `Seq` (lazy sequence)

And one type inspection builtin:

- **`type-of`** — returns a string: `"Int"`, `"Float"`, `"String"`,
  `"Bool"`, `"Null"`, `"Dict"`, `"Seq"`, `"Function"`, `"Builtin"`

### The Core Problem

In tinct, lists ARE dicts (Principle 1: Dicts Are Fundamental):

```lisp
[a b c]  ≡  [0: a  1: b  2: c]
```

There is no separate list type. A "list" is a dict with integer keys
`0, 1, 2, ...`. This unification means `type-of` returns `"Dict"` for
both `[a b c]` and `[name: Alice]`.

So what would `list?` mean? A dict whose keys are all integers
starting from 0? A dict with no string keys? A dict whose keys form a
contiguous 0..n range? The answer is that `list?` should not exist as a
builtin — "list-ness" is a convention, not a type distinction.

### Current Distinction Mechanisms

- **`type-of` returns `"Dict"` for both** — no distinction
- **`seq?` distinguishes lazy sequences** — `Seq` is a separate
  `Value` variant, not a dict
- **Key inspection at runtime** — users can check keys manually:
  ```lisp
  list?: [fn [xs]
    [and [= [type-of xs] "Dict"] [= [first [keys xs]] 0]]]
  ```

### What's Missing

1. **No per-type predicates.** Testing whether a value is an `Int` requires
   `[= [type-of x] "Int"]` — a string comparison against a magic constant.
2. **No boolean predicate for most types.** `seq?` exists but there is no
   `int?`, `str?`, `dict?`, `null?`, etc.
3. **Pattern matching foundation.** Type predicates are the first step
   toward pattern matching (see `doc/whatif/pattern-matching.md` Phase 1).

## What Type Predicates Would Provide

1. **Direct type dispatch.** `[int? x]` instead of
   `[= [type-of x] "Int"]` — cleaner, no magic strings, no risk of typos.
2. **Foundation for pattern matching.** Type predicates are Phase 1 of the
   pattern matching roadmap. Guards like `[if [int? x] ...]` are the
   primitive from which match expressions are built.
3. **Consistency with `seq?`.** Every `Value` variant gets a corresponding
   predicate, not just `Seq`.
4. **Foundation for type narrowing.** If path-sensitive narrowing is
   adopted (see `doc/whatif/narrowing.md`), type predicates provide the
   condition patterns that trigger narrowing — `[if [int? x] ...]`
   narrows `x` to `Int` in the true branch.

## Design

Add one type predicate builtin per `Value` variant (excluding `Seq`,
which already has `seq?`):

```
int?    : Any → Bool
float?  : Any → Bool
num?    : Any → Bool    # true for Int or Float
str?    : Any → Bool
bool?   : Any → Bool
null?   : Any → Bool
dict?   : Any → Bool    # true for both [a b c] and [name: Alice]
fn?     : Any → Bool    # true for Function and Builtin
```

**No `list?`** because:
1. Lists are dicts — there is no `Value::List` variant
2. Any definition of "list-ness" is arbitrary (dense integers? contiguous?
   starting from 0?)
3. Users who need array-vs-record distinction can write it as a stdlib
   function using `keys` + `every?`

### Semantics

Each predicate materializes its argument (forcing the thunk) and checks the
`Value` variant. This matches `seq?`'s existing behavior.

```rust
// Implementation pattern (same for all predicates)
fn builtin_int_q(args: &[Thunk], env: &Env) -> Result<Value> {
    let val = args[0].force()?;
    Ok(Value::Bool(matches!(val, Value::Int(_))))
}
```

`num?` is the only predicate that checks multiple variants — it returns
`true` for both `Value::Int` and `Value::Float`. This is a convenience
predicate that mirrors the `Number` supertype in the type system.

`fn?` returns `true` for both `Value::Function` and `Value::Builtin`,
since both are callable. Users who need to distinguish closures from
builtins can use `type-of`, which returns `"Function"` vs `"Builtin"`.

### Type Checker Integration

In the current type system, all predicates have type `Any → Bool`. When
bidirectional typing is adopted, they could serve as narrowing witnesses:
the type checker recognizes `[if [int? x] ...]` and narrows `x` to
`Int` in the true branch (see `doc/whatif/narrowing.md` Pattern 2). This
requires no changes to the predicates themselves — only to the type
checker's condition analysis.

### Interaction with Lazy Evaluation

Type predicates force their argument (strict in the doc/08-evaluation.md §Selective
Materialization sense). This is the same behavior as `seq?` and `type-of`.
Forcing is necessary because the type of an unevaluated thunk is not known
until materialization.

## What Would Change

### Builtins (`src/builtins.rs`)

**Current:** Only `seq?` and `type-of` exist for type inspection.
**Proposed:** Add 8 new builtin functions (`int?`, `float?`, `num?`,
`str?`, `bool?`, `null?`, `dict?`, `fn?`), each following the same
pattern as `seq?`: materialize argument, check variant, return bool.
**Impact:** Minor — 8 small functions, no new infrastructure. Each is
~5 lines of Rust.

### Builtin Type Signatures

**Current:** `seq?` typed as `Any → Bool`.
**Proposed:** All new predicates typed as `Any → Bool`. No type system
changes needed.
**Impact:** Minor — signature registration only.

### Parser / Grammar

**Current:** No changes needed — predicates are builtins, not syntax.
**Proposed:** No changes.
**Impact:** None.

### Evaluator (`src/eval.rs`)

**Current:** No changes needed — builtins are dispatched through the
existing builtin call mechanism.
**Proposed:** No changes.
**Impact:** None.

### Stdlib (`stdlib/prelude.llt`)

**Current:** No type predicates in stdlib.
**Proposed:** Optionally add `list?` as a stdlib function (not a builtin)
that checks for dict-with-integer-keys convention:
```tinct
list?: [fn [xs]
  [and [dict? xs] [every? [fn [k] [int? k]] [keys xs]]]]
```
**Impact:** Minor — optional convenience function, not a language change.

## Phased Adoption

### Phase 1: Core Predicates

Add `int?`, `float?`, `num?`, `str?`, `bool?`, `null?`, `dict?`,
`fn?` as builtins. Each is a standalone function with no dependencies on
other features.

### Phase 2: Narrowing Integration

When path-sensitive narrowing is adopted (see `doc/whatif/narrowing.md`),
type predicates become narrowing triggers: `[if [int? x] ...]` narrows
`x` to `Int` in the true branch. This requires type checker changes but
no changes to the predicates themselves.

### Prerequisites

- Phase 1 has no prerequisites — predicates are independent builtins that
  can be added at any time.
- Phase 2 requires `bidirectional-typing` and `narrowing` adoption.

### Trigger

- Adopt Phase 1 when pattern matching work begins
  (`doc/whatif/pattern-matching.md` Phase 1 depends on type predicates).
- Adopt Phase 1 independently when `type-of` string comparisons appear
  frequently in user code — predicates eliminate the magic-string
  anti-pattern.

## References

- doc/01-introduction.md §Principle 1: Dicts Are Fundamental — "A list is equivalent
  to a dict with integer keys." Motivates the absence of `list?`.
- doc/whatif/pattern-matching.md §Phase 1 — type predicates as the
  first step toward pattern matching.
- doc/whatif/narrowing.md — type predicates as narrowing condition patterns
  for path-sensitive type refinement.
- Tobin-Hochstadt, S. & Felleisen, M. (2010). "Logical types for untyped
  languages." In *ICFP '10*, pp. 117–128. ACM.
  — Occurrence typing: type predicates as the primitive for flow-sensitive
  type narrowing. Foundational model for Phase 2 narrowing integration.
