# Advanced Typeclass Extensions

> **Supersedes:**
>
> - `doc/06-type-inference.md §Constrained Type Variables / §Primitive Built-in Constraints` — the "Current Limitations" section is removed; §Multi-Parameter Type Classes replaces it with the MPTC design; §Constraint Propagation over BAS Types replaces the "No constrained row variables" limitation
> - `doc/07-type-extensions.md §Type System Extension Roadmap` — arithmetic type description updated from `Fn(Number, Number → Number)` to MPTC form
> - `doc/07-type-extensions.md §Dual-Dispatch Builtins` — user-extensibility via ClassEnv dispatch added
> - `doc/11a-builtins.md §Arithmetic` and `§Comparison` — both updated to reflect MPTC dispatch and user-defined type extensibility

Tinct's typeclass system is fully open: user-defined types participate in primitive operators, constraints propagate automatically over record fields, and arithmetic is precisely typed across mixed numeric modes.

## Precise Mixed-Mode Arithmetic

Arithmetic operators use a 3-parameter `Add a b c` class where `(a, b)` determines `c`. Mixed Int/Float expressions infer the correct result type without annotations:

```tinct
[+ 1 2.0]      # : Float   (Add Int Float Float)
[+ 1.5 2]      # : Float   (Add Float Int Float)
[+ 1 2]        # : Int     (Add Int Int Int)
[+ 1.5 2.5]    # : Float   (Add Float Float Float)
```

The same applies to `-`, `*`, and `/`. Division always produces `Float` when at least one operand is `Float`.

Functions over mixed types infer a polymorphic signature:

```tinct
scale: [fn [x rate] [* x rate]]
# : Add a b c => Fn@c [a b]

[scale 10 2]      # c = Int   (Add Int Int Int)
[scale 10 2.5]    # c = Float (Add Int Float Float)
```

The result type flows through the rest of the pipeline — no `@Unknown` annotation needed.

## Constraint Propagation Over Records

Constraints distribute over record fields automatically. A function annotated `constraint: [a: Equatable]` accepts any record whose fields are all equatable:

```tinct
deep-eq: [fn@[return: Bool  constraint: [a: Equatable]] [x@a y@a]
  [= x y]]

[deep-eq 42 42]                           # Equatable Int ✓
[deep-eq "hello" "world"]                 # Equatable Str ✓
[deep-eq {name: "Alice" age: 30}          # Equatable {name:Str} ∧ Equatable {age:Int} ✓
         {name: "Bob"   age: 25}]
[deep-eq {f: [fn [x] x]}                 # Equatable Fn → ✗
         {f: [fn [x] x]}]               # type error: Fn is not Equatable
```

**Union constraint propagation.** For union types, ALL alternatives must satisfy the constraint — because at runtime the value could be either:

```tinct
# Int | Str is Equatable (both Int and Str are Equatable)
compare: [fn@[return: Bool  constraint: [a: Equatable]] [x@[or Int Str] y@[or Int Str]]
  [= x y]]

# Int | Fn is NOT Equatable (Fn is not Equatable)
bad: [fn@[return: Bool  constraint: [a: Equatable]] [x@[or Int Fn] y@[or Int Fn]]
  [= x y]]   # type error
```

**Gradual boundaries.** A field annotated `@Unknown` satisfies any constraint — the check defers to runtime ClassEnv dispatch. If no instance is registered for the runtime type, primitive fallback handles it (for primitive types) or an error is raised.

## User-Defined Types in Primitive Operators

A user-defined type participates in `=`, `<`, `str`, and arithmetic by declaring instances for the relevant classes. Instance methods are invoked automatically by the primitive operators.

### Equatable and Comparable

```tinct
[type Priority [level@Integer]]

[EquatablePriority: [instance [Equatable Priority]
  [=:   [fn [a b] [= a.level b.level]]]
  [not=: [fn [a b] [not [= a b]]]]]]

[ComparablePriority: [instance [Comparable Priority]
  [<: [fn [a b] [< a.level b.level]]]
  [>: [fn [a b] [> a.level b.level]]]
  [<=: [fn [a b] [<= a.level b.level]]]
  [>=: [fn [a b] [>= a.level b.level]]]]]

p1: [Priority 1]
p2: [Priority 3]

[= p1 p2]      # false  — dispatches to EquatablePriority.=
[< p1 p2]      # true   — dispatches to ComparablePriority.<
[sorted [list p2 p1]]  # [[Priority 1] [Priority 3]]  ✓
```

`Comparable` entails `Equatable` — declaring `ComparablePriority` automatically makes `Priority` equatable without a separate `EquatablePriority` declaration.

### Castable (replaces Showable)

```tinct
[type Color [r@Integer g@Integer b@Integer]]

[instance Castable [let target@String source@Color]
  [cast: [fn [let c] [str-parts "rgb(" [@String [cast c.r]] "," [@String [cast c.g]] "," [@String [cast c.b]] ")"]]]]

red: [Color 255 0 0]
[@String [cast red]]    # "rgb(255,0,0)"  ✓
[str red]               # same, via str convenience alias
```

### Custom Arithmetic Types

A user-defined numeric type participates in arithmetic by declaring `Add`/`Sub`/`Mul`/`Div` instances:

```tinct
[type Decimal [value@String]]   # exact decimal represented as string

[AddDecimal: [instance [Add Decimal Decimal Decimal]
  [+: decimal-add]
  [-: decimal-sub]
  [*: decimal-mul]
  [/: decimal-div]]]

d1: [Decimal "1.5"]
d2: [Decimal "2.3"]
[+ d1 d2]   # : Decimal  — dispatches to decimal-add ✓
```

## Superclass Entailment

Declaring a superclass instance automatically satisfies its superclasses:

```tinct
# Comparable entails Equatable
[ComparablePriority: [instance [Comparable Priority] ...]]

# These all work without a separate EquatablePriority instance:
[= p1 p2]   # ✓ — Equatable is derived from Comparable
[not= p1 p2] # ✓
```

The built-in superclass hierarchy: `Comparable` extends `Equatable`.

## Instance Coherence

Two instances for the same type and class are rejected. For multi-parameter classes (`Add a b c`), coherence is checked per determining-position tuple: two `Add Int Float _` instances with different result types are a coherence violation regardless of the result type.

```tinct
[AddIntInt1: [instance [Add Int Int Int] ...]]
[AddIntInt2: [instance [Add Int Int Int] ...]]  # error: overlapping instance
```

## Gradual Typing Boundaries

When a value passes through a gradual boundary (`@Unknown`) and is then used with `=` or `str`, the ClassEnv dispatch fires at runtime. If a user instance is registered for the value's runtime type, it is used. If no instance exists and the type is not a built-in primitive, an error is raised.

This is the expected gradual typing behavior — `@Unknown` opts out of static constraint checking but retains runtime correctness via ClassEnv dispatch.
