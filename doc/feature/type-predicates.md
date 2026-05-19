# Type Predicates

## Overview

tinct provides per-type predicate builtins that test a value's runtime type
directly, replacing string comparisons against `type-of` output. `[int? x]`
is cleaner than `[= [type-of x] "Int"]`, eliminates magic strings, and serves
as the foundation for pattern matching guards and future type narrowing.

Every `Value` variant has a corresponding predicate. `seq?` already exists;
the full set is complete.

## Design

One type predicate builtin per `Value` variant:

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
   function using `keys` + `all?` — see `list?` in the standard library

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
since both are callable. `type-of` returns `"Function"` for both — there
is no runtime distinction between user-defined closures and builtins via
`type-of` or `fn?`.

### Type Checker Integration

In the current type system, all predicates have type `Any → Bool`. When
bidirectional typing is adopted, they serve as narrowing witnesses:
the type checker recognizes `[if [int? x] ...]` and narrows `x` to
`Int` in the true branch (see `doc/06-type-inference.md §Type Narrowing`). This
requires no changes to the predicates themselves — only to the type
checker's condition analysis.

### Interaction with Lazy Evaluation

Type predicates force their argument (strict in the doc/08-evaluation.md §Selective
Materialization sense). This is the same behavior as `seq?` and `type-of`.
Forcing is necessary because the type of an unevaluated thunk is not known
until materialization.

## Implementation

### Builtins (`src/builtins.rs`)

8 builtin functions (`int?`, `float?`, `num?`, `str?`, `bool?`, `null?`,
`dict?`, `fn?`), each following the same pattern as `seq?`: materialize
argument, check variant, return bool. Each is ~5 lines of Rust.

### Builtin Type Signatures

All predicates are typed as `Any → Bool`. No type system changes needed.

### Parser / Grammar

No changes needed — predicates are builtins, not syntax.

### Evaluator (`src/eval.rs`)

No changes needed — builtins are dispatched through the existing builtin call
mechanism.

### Stdlib (`stdlib/prelude.llt`)

`list?` is a stdlib function (not a builtin) that checks for the
dict-with-integer-keys convention:

```tinct
list?: [fn [xs]
  [and [dict? xs] [all? [fn [k] [int? k]] [keys xs]]]]
```

## References

- doc/01-introduction.md §Principle 1: Dicts Are Fundamental — "A list is equivalent
  to a dict with integer keys." Motivates the absence of `list?`.
- doc/14-patterns.md — type predicates integrate with pattern matching.
- doc/06-type-inference.md §Type Narrowing — type predicates as narrowing
  condition patterns for path-sensitive type refinement.
- Tobin-Hochstadt, S. & Felleisen, M. (2010). "Logical types for untyped
  languages." In *ICFP '10*, pp. 117–128. ACM.
  — Occurrence typing: type predicates as the primitive for flow-sensitive
  type narrowing. Foundational model for narrowing integration.
