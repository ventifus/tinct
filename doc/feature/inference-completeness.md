# Inference Completeness

> **Supersedes:**
> - `doc/06-type-inference.md §Limitations` items 2 ("Variadic params typed as Unknown") and 3 ("Nested dicts do not receive full let-polymorphism") — both are removed; the features described here replace them
> - The [FN] rule's variadic arm in `doc/06-type-inference.md §Inference Judgments` — updated from `Unknown` to `Seq(β)`; formal rules [FN-VARIADIC] and [CALL-VARIADIC] added

Tinct's type inference handles three patterns that arise naturally in idiomatic code: polymorphic dict entries that work across multiple types, polymorphic access through nested dict namespaces, and typed variadic functions.

## Polymorphic Dict Entries

A dict whose entries are independent (no entry references another by name) is inferred with each entry generalized independently. A polymorphic function defined in a dict can be used at different types by its siblings:

```tinct
utils: [
  id:     [fn [x] x]
  double: [fn [x] [+ x x]]
]

a: [utils.id 42]        # : Int
b: [utils.id "hello"]   # : Str
c: [utils.double 5]     # : Int
```

Mutually recursive entries form a single group and are inferred together. Each entry in a recursive group constrains the others, so they remain monomorphic relative to each other — the same as any ML `let rec`:

```tinct
[
  even: [fn [n] [if [= n 0] true  [odd  [- n 1]]]]
  odd:  [fn [n] [if [= n 0] false [even [- n 1]]]]
  # even : Fn@Bool [Int], odd : Fn@Bool [Int]
  # correctly monomorphic — they call each other
]
```

**Polymorphic recursion** (a function that calls itself at a different type than its definition) requires an explicit return type annotation. Without one, the type checker emits an error:

```tinct
# Error: recursive function requires an explicit return type annotation
bad: [fn [x] [bad [bad x]]]

# OK: annotation resolves the ambiguity
good: [fn@Int [x@Int] [good [good x]]]
```

## Polymorphic Access Through Visible Nested Dicts

When a dict literal is bound to a name in scope, dot-access retrieves the full polymorphic type scheme of each field. This makes named dicts work as polymorphic namespaces:

```tinct
math: [
  id:    [fn [x] x]          # ∀a. a → a
  const: [fn [x _] x]        # ∀a b. a → b → a
]

x: [math.id 42]              # : Int ✓
y: [math.id "hello"]         # : Str ✓
z: [math.const 42 "ignore"]  # : Int ✓
```

This applies when the dict is a **visible literal** — a name bound to a dict expression in the same or enclosing scope. Function parameters and values from other modules are opaque:

```tinct
# Opaque: only the declared type is available, not the full scheme
use-math: [fn [m@[id: Fn@a [a]]] [m.id 42]]
# m.id : Fn(a→a) at the declared type — a is fixed, not re-generalized per call
```

## Typed Variadic Parameters

`...args` collects remaining arguments into a `Seq(T)` where `T` is inferred from the call site. The variadic parameter is a standard Seq and supports all Seq operations directly:

```tinct
sum: [fn [...nums]
  [reduce [fn [acc n] [+ acc n]] 0 nums]]

[sum 1 2 3]        # nums : Seq(Int),   result : Int ✓
[sum 1.5 2.5 3.0]  # nums : Seq(Float), result : Float ✓
[sum 1 "two" 3]    # type error: variadic argument 2 has type Str, expected Int ✗
```

All variadic arguments must have the same type (or a common base type). `[sum 1 2.0]` succeeds with `Seq(Number)` — Int and Float both widen to Number when mixed.

Variadic parameters can be annotated with constraints:

```tinct
sum: [fn@[return: α  constraint: [α: Numeric]] [...nums]
  [reduce [fn [acc n] [+ acc n]] 0 nums]]
```

Without an annotation, the constraint is inferred from the body: if `nums` elements are used with `[+]`, the `Numeric` constraint propagates automatically.

### Heterogeneous Variadics

When argument types vary by position — the printf pattern — use a recursive typeclass chain:

```tinct
[FormatResult: [class [r@*]
  [apply-fmt: [fn@r [template@Str collected@[Seq Str]]]]]]

[FormatStr: [instance [FormatResult Str]
  [apply-fmt: [fn [t args] [str-format t args]]]]]

[FormatFn: [instance [FormatResult r  constraint: [r: FormatResult]]
               [FormatResult [fn@r [a@Str]]]
  [apply-fmt: [fn [t args] [fn [x] [apply-fmt t [conj args x]]]]]]]

format: [fn@[return: r  constraint: [r: FormatResult]] [template@Str]
  [apply-fmt template []]]
```

Each argument application peels one layer from the chain:

```tinct
[format "%d items"]         # : Fn@Str [Int]
[format "%d items: %s"]     # : Fn@Str [Int Str]
[[format "%d items" 42]]    # : Str → "42 items"
```
