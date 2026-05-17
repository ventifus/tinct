# CHR-Unified Type Constraints

> **Updates:**
> - `doc/06-type-inference.md §Multi-Parameter Type Classes and Functional Dependencies` — replaces the hardcoded 9-entry arithmetic table with the full CHR design
> - `doc/06-type-inference.md §Typeclass Declarations and Instances` — syntax updated for two-bracket class body and match-arm instances
> - `doc/feature/advanced-typeclasses.md §Precise Mixed-Mode Arithmetic` — arithmetic classes are now declared in `stdlib/prelude.llt`, not hardcoded in Rust

Tinct's constraint system is grounded in **Constraint Handling Rules** (CHRs), which unify functional dependencies and type-stage functions into one coherent mechanism. Users can declare multi-parameter type classes with functional dependencies, extend arithmetic to custom numeric types, and write type-level computations in tinct itself.

## Class Declarations

Classes are declared with `[class [params] [structural-metadata] methods...]`. The second positional bracket carries structural metadata — functional dependencies, resolver, kind constraints, and superclasses — and is omitted for simple classes:

```tinct
# Simple class — no structural metadata needed
Equatable: [class [a]
  eq?: [fn@Bool [a a]]]

# Class with functional dependency: (a, b) determines c
Addable: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [a b]]]

# HKT class — f is a type constructor (* → *)
Functor: [class [f]  [kinds: [f: Operator]]
  fmap: [fn@[return: [f b]] [g@[Fn@b [a]]  xs@[f a]]]]

# Superclass relationship
Comparable: [class [a]  [superclasses: [Equatable]]
  lt?: [fn@Bool [a a]]]
```

**`determines:`** declares a functional dependency as a two-element list `[[determining-vars] determined-var]`. Multiple FDs or multi-output FDs are supported:

```tinct
# Single FD: (a, b) → c
Add: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]  ...]

# Multi-output FD: (a, b) → (q, r) simultaneously
DivMod: [class [a b q r]  [determines: [[[a b] [q r]]]  resolver: DivModResult]  ...]

# Bidirectional FDs: a → b, and b → a
Convert: [class [a b]  [determines: [[[a] b]  [[b] a]]  resolver: [AtoB BtoA]]  ...]
```

**`resolver:`** names the type-stage function that computes the determined type(s) from the determining types. See §Type-Stage Resolvers below.

**`kinds:`** declares TypeVar kinds inline, replacing the `f@Operator` annotation form:

```tinct
Traversable: [class [t]  [kinds: [t: Operator]  superclasses: [Functor  Foldable]]
  traverse: [fn@[bind: [f]  kinds: [f: Operator]  constraint: [f: Applicative]
                 return: [f [t b]]]
             [g@[Fn@[return: [f b]] [a]]  xs@[t a]]]]
```

## Instance Declarations

Instances use match-arm syntax. Each arm pairs a `[pattern [...]]` type-parameter pattern with a method dict:

```tinct
[instance Addable
  [pattern [a@Int   b@Int   c@Int  ]]: [+: [fn@Int   [x@Int   y@Int  ] [builtin-add x y]]]
  [pattern [a@Int   b@Float c@Float]]: [+: [fn@Float [x@Int   y@Float] [builtin-add x y]]]
  [pattern [a@Float b@Int   c@Float]]: [+: [fn@Float [x@Float y@Int  ] [builtin-add x y]]]
  [pattern [a@Float b@Float c@Float]]: [+: [fn@Float [x@Float y@Float] [builtin-add x y]]]]

[instance Functor
  [pattern [f@Seq  ]]: [fmap: [fn@[return: [Seq b]] [g@[Fn@b [a]]  xs@[Seq a]] [map g xs]]]
  [pattern [f@Maybe]]: [fmap: [fn@[return: [Maybe b]] [g@[Fn@b [a]]  m@[Maybe a]]
                  [match m  [Some v]: [Some [g v]]  None: None]]]]
```

**Pattern forms inside `[pattern [...]]`:**

| Form | Meaning |
|------|---------|
| `a@Int` | Class param `a` must be `Int` |
| `f@Seq` | Class param `f` must be `Seq` (bare constructor for HKT params) |
| `a@[Seq elem]` | Class param `a` must be `Seq` of fresh TypeVar `elem` |
| `a@[Map k v]` | Class param `a` must be `Map` from `k` to `v` |

Instances are anonymous — they register automatically and are selected at every call site that matches. No instance can overlap with another (the type checker rejects conflicting arms at declaration time).

## Functional Dependency Inference

When a class has a functional dependency `(a, b) → c`, the type checker automatically determines `c` from `a` and `b` at each call site. No annotation needed:

```tinct
[+ 1 2]      # a=Int, b=Int   → c=Int   (Add Int Int Int)
[+ 1 2.0]    # a=Int, b=Float → c=Float (Add Int Float Float)
[+ 1.5 2.5]  # a=Float, b=Float → c=Float

scale: [fn [x rate] [* x rate]]
# Inferred: ∀a b c. Mul a b c ⇒ Fn@c [a b]
[scale 10 2]    # c = Int
[scale 10 2.5]  # c = Float
```

The FD constraint travels in the type scheme: `scale` is polymorphic, and the result type is determined fresh at each call site when the arg types become concrete.

**Comparing two arithmetic results** works naturally — the type checker defers equality checking until both sides reduce to concrete types:

```tinct
[= [+ 1 2.0] [+ 1.5 2.5]]  # both produce Float → type checks ✓
[= [+ 1 1]   [+ 1.5 2.5]]  # Int vs Float → type error ✗
```

## Type-Stage Resolvers

A resolver is a function in a `--- stage: type` section that receives determining type dicts and returns the determined type dict. It runs at type-check time, not at runtime:

```tinct
--- stage: type
[
  AddResult: [fn [...args]
    [match [[builtin-get 0 args]  [builtin-get 1 args]]
      [[kind: "named" name: "Int"]    [kind: "named" name: "Int"]]:   [kind: "named" name: "Int"]
      [[kind: "named" name: "Int"]    [kind: "named" name: "Float"]]: [kind: "named" name: "Float"]
      [[kind: "named" name: "Float"]  [kind: "named" name: "Int"]]:   [kind: "named" name: "Float"]
      [[kind: "named" name: "Float"]  [kind: "named" name: "Float"]]: [kind: "named" name: "Float"]
      _:                                                               [kind: "named" name: "Unknown"]]]
]
---
Addable: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [a b]]]
```

**Type dict schema** — each type is represented as a dict with a `kind:` key:

| Type | Dict representation |
|------|-------------------|
| `Int` | `[kind: "named" name: "Int"]` |
| `Str` | `[kind: "named" name: "Str"]` |
| `Bool` | `[kind: "named" name: "Bool"]` |
| `Float` | `[kind: "named" name: "Float"]` |
| `Unknown` | `[kind: "named" name: "Unknown"]` |
| `Seq T` | `[kind: "seq" element: <type-dict>]` |
| `Map K V` | `[kind: "map" key: K value: V]` |
| `Record {...}` | `[kind: "record" fields: {...}]` |
| `Union [T₁ T₂]` | `[kind: "union" members: [...]]` |

Resolvers may call other type-stage functions:

```tinct
--- stage: type
[
  NullableAddResult: [fn [...args]
    [or [AddResult [builtin-get 0 args] [builtin-get 1 args]]
        [kind: "named" name: "Null"]]]
]
```

**Multi-output resolvers** return `[kind: "multi-output" varname1: <type> varname2: <type>]`:

```tinct
--- stage: type
[
  DivModResult: [fn [...args]
    [match [[builtin-get 0 args]  [builtin-get 1 args]]
      [[kind: "named" name: "Int"]  [kind: "named" name: "Int"]]:
        [kind: "multi-output"
         q: [kind: "named" name: "Int"]
         r: [kind: "named" name: "Int"]]]]
]
---
DivMod: [class [a b q r]  [determines: [[[a b] [q r]]]  resolver: DivModResult]
  divmod: [fn@[record q: q  r: r] [a b]]]
```

## User-Defined Arithmetic Types

The arithmetic classes (`Addable`, `Subtractable`, `Multipliable`, `Divisible`) are declared in `stdlib/prelude.llt` using the same mechanism — there is no special-casing for built-in types. A user-defined `Decimal` type participates simply by adding instance arms:

```tinct
--- stage: type
[AddResult: [fn [...args] ...]]   # already in prelude
---
# Add Decimal instances alongside the existing Int/Float ones
[instance Addable
  [pattern [a@Decimal b@Decimal c@Decimal]]:
    [+: [fn@Decimal [x@Decimal y@Decimal] [decimal-add x y]]]]

# Now Decimal arithmetic infers the correct type:
[+ [decimal "1.5"] [decimal "2.3"]]  # : Decimal (Add Decimal Decimal Decimal)
```

## Config Merging with FDs

FDs are not limited to arithmetic. A `Merge` class with FD `(a, b) → c` expresses that merging two specific config schemas produces a third with the result type inferred:

```tinct
--- stage: type
[
  MergeResult: [fn [...args]
    [match [[builtin-get 0 args] [builtin-get 1 args]]
      [[kind: "record" fields: [host: _  port: _]]
       [kind: "record" fields: [timeout: _  retries: _]]]:
         [kind: "record"  fields: [host:    [kind: "named" name: "Str"]
                                   port:    [kind: "named" name: "Int"]
                                   timeout: [kind: "named" name: "Int"]
                                   retries: [kind: "named" name: "Int"]]]
      _: [kind: "named" name: "Unknown"]]]
]
---
Merge: [class [a b c]  [determines: [[[a b] c]]  resolver: MergeResult]
  merge: [fn@c [a b]]]

[instance Merge
  [pattern [a@ServerBase  b@ServerOpts  c@ServerFull]]:
    [merge: [fn [base opts] [host: base.host  port: base.port
                              timeout: opts.timeout  retries: opts.retries]]]]

# FD infers the result type from the argument types:
[merge server-base server-opts]  # : ServerFull — inferred, no annotation needed
```

## Instance Soundness

The type checker enforces three conditions on instance declarations (checked as a batch after all `[instance ...]` forms for the class are processed):

1. **Disjointness** — no two arms match the same ground type tuple. Overlapping arms are rejected with a diagnostic naming both conflicting arms.

2. **Coverage** (for FD classes) — every variable in the determined position must appear in the determining positions. Prevents improvement from introducing unknowns that cannot be resolved.

3. **Consistency** (for FD classes) — if two arms' determining positions could unify, their determined types must agree. Prevents conflicting result types for the same input.

```tinct
# Rejected: consistency violation — both arms have (Int, Int) determining
#           positions but different determined types (Int vs Float)
[instance Addable
  [pattern [a@Int b@Int c@Int  ]]: [+: ...]
  [pattern [a@Int b@Int c@Float]]: [+: ...]]  # error
```

## Automatic Boundary Guards

Every point where an `Unknown`-typed value crosses into a context expecting a concrete type receives a runtime guard. The type checker's post-inference elaboration pass inserts these automatically — explicit `[@Type expr]` TypeAsserts are not required at every boundary:

```tinct
data: [from-json input]   # data: Unknown
port: data.port           # port: Unknown

server: [start port]      # if start expects Int, a guard fires here at runtime:
                          # type mismatch: expected Int, got String
                          # blame: value from line 2 (from-json result, Unknown type)
```

See [Formal Gradual Typing](gradual-typing.md) §Automatic Boundary Guards for the blame strategy and boundary catalog.

## Error Messages

**FD improvement failure** — when no instance matches the ground arg types:

```
no Add instance for (Bool, String)
  at: [+ flag message]  line 7
  registered instances: (Int, Int), (Float, Float), (Int, Float), (Float, Int)
```

**Depth limit exceeded** — when a resolver's type-stage evaluation hits the recursion limit:

```
type-stage reduction depth exceeded while computing MergeResult(ServerBase, ServerOpts)
  at: [merge base opts]  line 12
  check resolver for infinite recursion or increase --type-stage-depth
```

**Instance coherence violation** — at class declaration time:

```
consistency violation for class Add:
  arm [pattern [a@Int b@Int c@Int]] at line 5
  arm [pattern [a@Int b@Int c@Float]] at line 6
  both match determining positions (Int, Int) but disagree on c: Int vs Float
```

## References

- Sulzmann, M., Duck, G.J., Peyton Jones, S. & Stuckey, P.J. (2007). "Understanding Functional Dependencies via Constraint Handling Rules." *JFP* 17(1), 83–129.
- Jones, M.P. (1995). *Qualified Types: Theory and Practice.* Cambridge University Press.
- Jones, M.P. (2000). "Type Classes with Functional Dependencies." *ESOP 2000.*
- Schrijvers, T. et al. (2009). "Complete and Decidable Type Inference for GADTs." *ICFP '09.* — OutsideIn(X) touchability; deferred equality for non-injective resolvers.
- Eisenberg, R.A. et al. (2014). "Closed Type Families with Overlapping Equations." *POPL 2014.* — apartness rule for distinct type families.
