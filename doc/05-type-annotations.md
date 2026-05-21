# Type Annotations & Type Expressions

**Mandatory, bottom-up type inference with annotation-driven polymorphism, inspired by Hindley-Milner.** Every value has a type. Type errors raised early — good for LLMs and LSP feedback. Annotations are optional but enforced: write one and the compiler verifies it.

---

## Part I: Foundations

### 1. The `@` Concept

**`@` attaches a type or property dict** to a name, function, or expression. It is always a structural separator — not a valid identifier character. Wherever `@` appears immediately after a bare word (no whitespace), it separates the word from its annotation.

```tinct
x@Int                       # parameter x has type Int
fn@String                   # function returns String
[@Int expr]                 # type assertion: expr must be Int at runtime
=== error
type errors:
  expected record type, got Int at 1:1-1:6
  expected record type, got String at 2:1-2:10
  undefined variable: expr at 3:7-3:11

```

Two forms:

- **`name@Type`** — annotation on a binding or parameter
- **`[@Type expr]`** — type assertion on an expression

Strings containing `@` must be quoted: `"email@example.com"`.

### 2. Simple Type Annotations

`x@Type` declares the compile-time type of parameter `x`. If the annotation is a bare name, it is a type reference (uppercase = concrete, lowercase = TypeVar). If it is a bracket expression, it is resolved as a type-stage expression.

```tinct
x@Int                       # x has type Int
x@String                    # x has type String
x@a                         # x has TypeVar type a (polymorphic)
x@[or Int Null]             # x has union type Int | Null
x@[host: String  port: Int] # x has record type
=== error
type errors:
  expected record type, got Int at 1:1-1:6
  expected record type, got String at 2:1-2:9
  expected record type, got _t0 at 3:1-3:4
  expected record type, got Int | [] at 4:1-4:16

```

**Annotations are contracts.** The compiler infers types when omitted. If you write an annotation, the inferred type must match — a mismatch is a type error, not a coercion.

**Any parameter is nameable at the call site** (Kotlin model). A parameter with `default:` is optional; without it, the parameter is required.

```tinct
fetch: [fn@String [url@String
                   timeout@[type: Int  default: 30]
                   retries@[type: Int  default: 3]]
  ...]

[fetch "https://example.com" timeout: 60]
# url = "https://example.com", timeout = 60, retries = 3
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 3:1:6
  |
  1 | fetch: [fn@String [url@String
    |      ^
```

**Type conventions:**

- Uppercase first letter: concrete types (`Int`, `String`, `Bool`, `Null`, `Any`)
- Lowercase first letter: type variables (`a`, `b`, `k`, `v`)
- `String` / `Str`: `String` is the user-facing annotation name; `Str` is the internal `Type::Str` variant. Use `String` in annotations; `Str` appears in error messages.
- `Null`: the empty record `[]` — closed record with no fields. Use `fn@Null` for functions returning no meaningful value.
- `Any`: dynamic escape hatch — accepts any value, no static checking.
- `Unknown`: gradual type (`?`) — like `Any` for inference but propagates through type checking. Unconstrained inference positions resolve to `Unknown`.

### 3. Type Assertions

**`[@Type expr]` asserts the type of an expression at runtime.** Materializes the value, checks its type, and throws `TypeError` on mismatch.

```tinct
data: [from-json input]           # type: Any

name: [@String data.name]         # throws if data.name is not a String
port: [@Int    data.port]         # throws if data.port is not an Int

# Inline
[+ [@Int x] 1]

# With fallback — safe cast
port: [@[type: Int  default: 8080] config.port]
# Returns 8080 if config.port is absent or not Int
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 4:1:5
  |
  1 | data: [from-json input]           # type: Any
    |     ^
```

`[@Type expr]` is unambiguous: inside `[...]`, if the first token is `@`, it is always a type assertion. `@` cannot start a bare word or variable reference.

**`default:` meaning by context:**

| Context | `default:` meaning |
|---------|-------------------|
| Function parameter | Value used when caller omits the argument |
| `[@...]` assertion | Fallback if type assertion fails — no exception thrown |

---

## Part II: Type Complexity

### 4. Parameterized Types

Type constructors take type arguments via bracket application:

```tinct
xs@[Seq Int]               # Seq of Int
scores@[Map [String: Int]] # Map from String to Int
nested@[Seq [Seq Int]]     # Seq of Seq of Int
pair@[Pair Int String]     # Pair of Int and String (user-defined)
=== error
type errors:
  expected record type, got Seq[Int] at 1:1-1:13
  expected record type, got Map[_ [String: Int]] at 2:1-2:27
  expected record type, got Seq[Seq[Int]] at 3:1-3:23
  undefined type: Pair at 4:7-4:11

```

**Bare type constructors** produce unconstrained versions:

- `@Seq` → `Seq(Unknown)` — sequence with unconstrained element type
- `@Map` → `Map(Unknown, Unknown)` — map with unconstrained key and value
- `@Dict` → open record (fresh row variable) — any dict structure
- `@Fn` → variadic function returning `Any` — any callable

```tinct
# @Dict: each annotation creates an independent row variable
process: [fn [x@Dict  y@Dict] ...]   # x and y may have different shapes

# @[Seq Int] in assertion position
items: [@[Seq Int] [from-json input]]   # checks element type
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 6:2:8
  |
  2 | process: [fn [x@Dict  y@Dict] ...]   # x and y may have different shapes
    |        ^
```

Parameterized aliases use `[ConstructorName TypeArgs...]`:

```tinct
Either: [type [a b] [or a b]]

x@[Either Int String]     # resolves to Int | String
y@[Either a b]            # TypeVars — must be in bind: if in fn@[...]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 7:1:7
  |
  1 | Either: [type [a b] [or a b]]
    |       ^
```

**Type constructor variables — `@Operator` kind:**

A type parameter annotated with an `Operator`-kinded typeclass (`@Functor`, `@Monad`, `@Traversable`, etc.) ranges over type constructors of kind `* → *` — types that take one type argument, such as `Seq`, `Result`, and `Maybe`:

```tinct
# f@Functor: f ranges over Seq, Result, Maybe, ...
fmap: [fn@[f b] [f@Functor  fn@b [a]  [f a]]]

# m@Monad: m is a type constructor with a Monad instance
sequence: [fn@[m [Seq a]] [m@Monad  xs@[Seq [m a]]]
  [traverse m [fn [x] x] xs]]
```

In annotation positions, `@[m a]` applies type constructor `m` to type argument `a`. The disambiguation rule: bracket annotations without colons are type constructor application; with colons they are record types.

| Syntax | When valid | Meaning |
|--------|-----------|---------|
| `@[m a]` | `m` is an Operator-kinded variable | Apply constructor `m` to type `a` |
| `@[Seq Int]` | always | Sequence of `Int` (builtin shorthand) |
| `@[m [Seq a]]` | `m` is Operator-kinded | `m` applied to `Seq a` |
| `@[name: Str]` | always | Record type (colon present — not application) |

---

### 5. Union and Intersection Types

**Union — `or` type-stage combinator:**

```tinct
x@[or Int Null]           # Int | Null
x@[or String Int Bool]    # String | Int | Bool
fn@[return: [or Int Null]] [xs@[Seq Int]  target@Int] ...]
=== error
error: unmatched closing bracket
 --> block 8:3:58
  |
  3 | fn@[return: [or Int Null]] [xs@[Seq Int]  target@Int] ...]
    |                                                          ^
```

`or` is a type-stage function in the prelude. It produces `[kind: "union" members: [...]]` → `Type::Union(Vec<Type>)`. Union members are normalized: deduplicated, sorted, and flattened (nested unions collapse).

**Intersection — `each` type-stage combinator:**

```tinct
x@[each Comparable Showable]       # Comparable ∩ Showable
constraint: [a: [each Comparable Showable]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 9:2:11
  |
  2 | constraint: [a: [each Comparable Showable]]
    |           ^
```

`each` produces `[kind: "inter" members: [...]]` → `Type::Intersection(Vec<Type>)`. In `constraint:` position, each member becomes a separate `Constraint::Class`. In annotation position, it produces `Type::Intersection`.

**BAS annotation call-form — `@[[all A B]]` and `@[[without A]]`:**

The `all` and `without` type-stage functions are also available in the double-bracket annotation form for inline BAS types:

```tinct
x@[[all Comparable Showable]]    # intersection: Comparable ∩ Showable
x@[[without String]]             # negation: ~String (any type except String)
```

`@[[all T1 T2 ...]]` — the inner `[all T1 T2]` parses as `Expr::Call { func: VarRef("all"), args: [T1, T2, ...], implied: true }`. The annotation resolver dispatches on the `all` head and produces `Type::normalize_intersection([T1, T2, ...])`.

`@[[without T]]` — the inner `[without T]` likewise parses as a Call with head `without`, producing `Type::Negation(T)`.

The double-bracket form is equivalent to the single-bracket form using `each` or a negation-combinator:

| Double-bracket form | Equivalent single-bracket form |
|--------------------|-------------------------------|
| `@[[all A B]]` | `@[each A B]` |
| `@[[without A]]` | negation; no `each`-based equivalent |

**Subtyping rules (BAS):**

- `T <: Union(T, U)` — injection
- `Union(T, U) <: V` iff `T <: V` and `U <: V` — elimination
- `Intersection(T, U) <: T` and `Intersection(T, U) <: U` — projection
- `T <: ~A` holds conservatively (full BAS RDNF requires `T ∩ A = Never`)

### 6. Record Types

A record type is a dict of field names to types. Records are **closed by default** in tinct's BAS model — width subtyping provides structural openness without row variables.

```tinct
x@[name: String  age: Int]    # closed record: exactly these fields
x@[name: String]              # closed record: exactly {name: String}
=== error
type errors:
  expected record type, got [age: Int] & [name: String] at 1:1-1:27

```

**Width subtyping:** a function annotated `@[name: String]` accepts any record that has at least a `name: String` field — because any record with more fields is a subtype of one with fewer fields.

```tinct
# Accepts {name: String, age: Int, ...} and {name: String} alike
greet: [fn@String [p@[name: String]] [str "Hello " p.name]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 11:2:6
  |
  2 | greet: [fn@String [p@[name: String]] [str "Hello " p.name]]
    |      ^
```

**`...` rest syntax** in record annotations is accepted and expresses openness intent, but produces the same closed record type — width subtyping handles it automatically.

**`@Dict`** — open record with a fresh row variable per annotation site. Use when the function truly accepts any dict structure:

```tinct
count-keys: [fn@Int [d@Dict] [length [keys d]]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 12:1:11
  |
  1 | count-keys: [fn@Int [d@Dict] [length [keys d]]]
    |           ^
```

**Records with reserved field names** (`type:`, `return:`, `default:`, `doc:`, `is:`, `repr:`, `constraint:`, `bind:`, `kinds:`) must use a type alias:

```tinct
TypedItem: [type [type: String  id: Int]]   # alias for the record type
x@TypedItem
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 13:1:10
  |
  1 | TypedItem: [type [type: String  id: Int]]   # alias for the record type
    |          ^
```

### 7. Function Types

**`Fn@Return [ParamTypes]`** is the function type constructor. It mirrors function definitions:

```tinct
# Definition:   fn@Return [params] body
[fn@Int [x@Int y@Int] [+ x y]]

# Type:         Fn@Return [ParamTypes]
[Fn@Int [Int Int]]
=== error
type errors:
  expected record type, got Fn@Int [x: Int y: Int] at 2:1-2:31
  undefined variable: Int at 5:10-5:13

```

```tinct
[Fn@b [a]]                    # function from a to b
[Fn@Bool [a]]                 # predicate
[Fn@c [a b]]                  # two-arg function
[Fn@[return: [f b]] [[Fn@b [a]]  [f a]]]  # HKT: (a→b) → f a → f b
=== error
type errors:
  undefined variable: a at 1:8-1:9
  undefined variable: a at 2:11-2:12
  undefined variable: a at 3:8-3:9
  invalid type expression in annotation: [f b] at 4:14-4:19
  undefined variable: a at 4:29-4:30
  undefined variable: f at 4:35-4:36

```

`Fn` is uppercase (type constructor convention). The annotation after `@` is the return type; the bracket is the list of parameter types. All types must be explicit — there is no body to infer from.

**`@Fn`** (bare) — any callable: variadic function returning `Any`. Used for higher-order parameters where the specific signature is not constraining.

**Subtyping:** named function types are subtypes of anonymous ones by dropping parameter names (Gaster & Jones 1996). `Fn@Int [x: Int  y: Int]` <: `Fn@Int [Int Int]`.

---

## Part III: Polymorphism

### 8. TypeVars and `bind:`

**`bind:` is the sole TypeVar declaration site** in `fn@[...]` annotations. It declares names as fresh TypeVars, processed before all other keys. Lowercase names in annotation position are always TypeVars — never value references.

```tinct
# Single TypeVar
min: [fn@[bind: [a]  return: a  constraint: [a: Comparable]]
      [xs@[Seq a]] ...]

# Multiple TypeVars
zip: [fn@[bind: [a b]  return: [Seq [record left: a  right: b]]]
      [xs@[Seq a]  ys@[Seq b]] ...]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 16:2:4
  |
  2 | min: [fn@[bind: [a]  return: a  constraint: [a: Comparable]]
    |    ^
```

**TypeVar scoping rules:**

1. `bind: [a b c]` registers `a`, `b`, `c` as fresh TypeVars in `ann_mapping`
2. `return:`, `type:`, and parameter `@` annotations reference names from `ann_mapping`
3. A name in `return:` or a parameter annotation that is not in `bind:` is a type error
4. TypeVars are local to the function annotation — no outer-scope shadowing

```tinct
# a declared in bind:, referenced everywhere
transform: [fn@[bind: [a b]  return: [Seq b]  constraint: [a: Showable]]
             [xs@[Seq a]  f@[Fn@b [a]]] ...]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 17:2:10
  |
  2 | transform: [fn@[bind: [a b]  return: [Seq b]  constraint: [a: Showable]]
    |          ^
```

**Without `bind:`:** TypeVars introduced solely through `constraint:` keyed entries are also valid for single-TypeVar cases (backward compatible):

```tinct
# Equivalent — a introduced via constraint:
min: [fn@[return: a  constraint: [a: Comparable]] [xs@[Seq a]] ...]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 18:2:4
  |
  2 | min: [fn@[return: a  constraint: [a: Comparable]] [xs@[Seq a]] ...]
    |    ^
```

But when TypeVars appear only in MPTC positional entries (like `c` in `[$Addable a b c]`), `bind:` is required to declare them before use.

### 9. TypeVar Constraints

`constraint:` declares class constraints on TypeVars. Values are type-stage expressions evaluated in the type-stage Env.

```tinct
# Single constraint
constraint: [a: Comparable]

# Multiple TypeVars
constraint: [a: Comparable  b: Showable]

# Multiple constraints on one TypeVar — each combinator
constraint: [a: [each Comparable Showable]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 19:2:11
  |
  2 | constraint: [a: Comparable]
    |           ^
```

**Routing:** constraint values are type-stage expressions. `Comparable` resolves to `[kind: "named" name: "Comparable"]` → `Constraint::Class("Comparable", α)`. `[each Comparable Showable]` resolves to `[kind: "inter" members: [...]]` → two separate `Constraint::Class` entries.

**Interaction with inference.** Explicit constraints compose with inferred constraints. If `constraint: [a: Comparable]` is declared and the body also uses `a` in an `Equatable` context, both register; constraint simplification removes `Equatable a` since `Comparable` entails it via the superclass relation.

**`doc:` and LSP hover.** The doc string is stored in `TypeScheme.doc` and displayed below the inferred signature in LSP hover:

```
min: Comparable a => Fn@a [[Seq a]]
Return smallest element
```

**Examples:**

```tinct
min: [fn@[bind: [a]  return: a  constraint: [a: Comparable]  doc: "Return smallest element"]
      [xs@[Seq a]] ...]
# Inferred: Comparable a => Fn@a [[Seq a]]

compare: [fn@[bind: [a b]  return: Bool  constraint: [a: Comparable  b: Showable]]
          [x@a  y@a  logger@b] ...]
# Inferred: (Comparable a, Showable b) => Fn@Bool [a a b]

display-sorted: [fn@[bind: [a]  return: String  constraint: [a: [each Comparable Showable]]]
                 [xs@[Seq a]] ...]
# Inferred: (Comparable a, Showable a) => Fn@String [[Seq a]]

check-all: [fn@[bind: [a]  return: Bool  constraint: [a: Equatable]]
            [xs@[Seq a]  target@a] ...]
# Inferred: Equatable a => Fn@Bool [[Seq a] a]

between: [fn@[bind: [a]
              return: [Fn@Bool [a]]
              constraint: [a: Comparable]
              doc: "Return a predicate for [lo, hi)"]
          [lo@a  hi@a]
          [fn@Bool [x@a] [and [>= x lo] [< x hi]]]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 20:1:4
  |
  1 | min: [fn@[bind: [a]  return: a  constraint: [a: Comparable]  doc: "Return smallest element"]
    |    ^
```

### 10. Multi-Parameter Type Classes

**MPTC constraints** relate multiple TypeVars via a class with functional dependencies. Written as positional entries in `constraint:`:

```tinct
constraint: [a: Numeric  b: Numeric  [$Addable a b c]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 21:1:11
  |
  1 | constraint: [a: Numeric  b: Numeric  [$Addable a b c]]
    |           ^
```

`[$Addable a b c]` — the `$` sigil looks up `Addable` in the ClassEnv (not the type-stage Env). All three TypeVars (`a`, `b`, `c`) must be declared in `bind:`. The class's functional dependency `(a, b) → c` means: when `a` and `b` are known, `c` is determined.

```tinct
scale: [fn@[bind: [a b c]
             return: c
             constraint: [a: Numeric  b: Numeric  [$Multipliable a b c]]]
  [x@a  factor@b]
  [* x factor]]

[scale 10 2]        # a=Int, b=Int → c=Int via FD
[scale 10 2.0]      # a=Int, b=Float → c=Float via FD
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 22:1:6
  |
  1 | scale: [fn@[bind: [a b c]
    |      ^
```

**`kinds:` for higher-kinded TypeVars.** When a TypeVar is a type constructor (kind `Operator`), declare it with `kinds:`:

```tinct
fmap-generic: [fn@[bind: [a b f]
                   kinds: [f: Operator]
                   constraint: [f: Functor]
                   return: [f b]]
               [[Fn@b [a]]  xs@[f a]] ...]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 23:1:13
  |
  1 | fmap-generic: [fn@[bind: [a b f]
    |             ^
```

`kinds: [f: Operator]` registers `f` in `kind_env` as an Operator-kinded TypeVar. Kind names: `Operator` (type constructor `* → *`), `Label` (dict field key).

### 11. Label TypeVars and Field Access

`key@Label` annotates a parameter as a label (dict field key). The type checker generates a `HasField` constraint automatically:

```tinct
# Anonymous form
get-field: [fn@[return: a] [key@Label  dict@d] [get key dict]]
# inferred: ∀ (l : Label) d a. HasField l d a => Label(l) → d → a

# Named form — when the same label appears twice
get-or-default: [fn@[return: a] [key@[label: l]  default@a  dict@d] ...]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 24:2:10
  |
  2 | get-field: [fn@[return: a] [key@Label  dict@d] [get key dict]]
    |          ^
```

`HasField` constraints are never written explicitly — they arise from `key@Label` annotations. The type checker looks up the label type in the record type of `dict` and unifies with `a`.

---

## Part IV: Type-Stage Programming

### 12. `--- stage: type` Sections

**`--- stage: type` sections define type-level functions** evaluated at type-check time. They are syntactically ordinary LLT code; the `stage: type` header routes the document to the type-stage evaluator instead of the runtime evaluator.

```tinct
--- stage: type
[
  Nullable: [fn [t] [or t Null]]
  Pair:     [fn [a b] [record first: a  second: b]]
]
---
x@[Nullable Int]       # Int | Null
p@[Pair String Bool]   # {first: String  second: Bool}
=== error
type errors:
  undefined type: Nullable at 7:4-7:12
  undefined type: Pair at 8:4-8:8

```

**Isolation.** The type-stage Env is separate from the runtime Env. Type-stage functions cannot reference runtime bindings. The type-stage Env is built from:

1. Prelude `--- stage: type` section (including built-in type combinators)
2. Program `--- stage: type` sections in source order

The type-stage Env is discarded after type-checking; it does not exist at runtime.

**Available builtins in type-stage:** `builtin-get`, `get?`, `keys`, `length`, `=`, `if`, `match`. The higher-level `get` (which is a program-stage wrapper) is not available — use `builtin-get` instead.

**Recursive type-stage functions are not supported.** The lazy evaluator defers self-calls as thunks; the annotation resolver forces thunks during traversal, causing infinite unrolling until the 256-layer depth limit fires. Use type aliases for recursive structures.

### 13. Type Prelude and Type Constructors

The prelude `--- stage: type` section defines all built-in type combinators:

```tinct
--- stage: type
[
  or:     [fn [...types] [kind: "union"  members: types]]
  each:   [fn [...types] [kind: "inter"  members: types]]
  record: [fn [...fields] [kind: "record"  fields: fields]]

  Seq:    [fn [t]    [kind: "seq"    element: t]]
  Map:    [fn [k v]  [kind: "map"    key: k  value: v]]
  Fn:     [fn [ret ...params] [kind: "fn"  return: ret  params: params]]

  # Ground types
  Int:    [kind: "named"  name: "Int"]
  String: [kind: "named"  name: "String"]
  Bool:   [kind: "named"  name: "Bool"]
  Null:   [kind: "named"  name: "Null"]
  Any:    [kind: "named"  name: "Any"]
  Unknown:[kind: "named"  name: "Unknown"]
]
=== out
{}
```

**Name resolution order** in type-stage Env:

1. Type-stage bindings (`or`, `each`, user-defined)
2. Type alias table (aliases declared with `[type ...]`)
3. Primitive named types (`Int`, `String`, `Bool`, `Null`, `Any`, `Unknown`)

### 14. Annotation Resolution

Annotation brackets `@[...]` are resolved by evaluating their contents in the type-stage Env, then converting the resulting type dict to a `Type::*` via `dict_to_type()`.

```tinct
@[or Int Null]
# eval("or Int Null", type_stage_env) → [kind: "union"  members: [Int-dict  Null-dict]]
# dict_to_type → Type::Union([Type::Int, Type::Record(Empty)])

@[Seq Int]
# eval("Seq Int", type_stage_env) → [kind: "seq"  element: [kind: "named"  name: "Int"]]
# dict_to_type → Type::Seq(Type::Int)
=== error
error: @ annotations outside type-assert or param contexts not yet supported
 --> block 27:1:1
  |
  1 | @[or Int Null]
    | ^
```

**Disambiguation of bracket annotation contents:**

- Any keyed entry matching a metadata key (`bind:`, `return:`, `constraint:`, `kinds:`, `doc:`) → metadata dict (not a type expression)
- All-keyed with unrecognized keys → record type
- Mixed positional and keyed with recognized metadata keys → type error
- `or:` (colon-suffixed) → always a dict key, always a record field name
- `or` (bare in head position) → always a type-stage function call

```tinct
@[or: Int  port: Int]    # Record schema: fields "or" and "port"
@[or Int Null]           # Union type: Int | Null
=== error
error: @ annotations outside type-assert or param contexts not yet supported
 --> block 28:1:1
  |
  1 | @[or: Int  port: Int]    # Record schema: fields "or" and "port"
    | ^
```

**`TypeStageApp`** — when annotation brackets contain TypeVar arguments that are not yet ground, the resolver produces a lazy `TypeStageApp` node instead of evaluating eagerly. It reduces to a concrete type when the TypeVars are resolved during inference.

### 15. Type Alias Declarations

`[type ...]` declares a named type alias. The body is evaluated using the full annotation resolver with type-stage Env access.

```tinct
# Simple alias
NullableInt: [type [or Int Null]]
Name:        [type String]

# Parameterized alias
Either:  [type [a b] [or a b]]
Pair:    [type [a b] [record first: a  second: b]]
Scores:  [type [Map String: Int]]

# Use site
x@NullableInt
y@[Either Int String]    # a=Int, b=String substituted directly
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 29:2:12
  |
  2 | NullableInt: [type [or Int Null]]
    |            ^
```

**Parameterized alias use:** `x@[Either Int String]` substitutes `a=Int`, `b=String` directly — it is a substitution, not instantiation of fresh TypeVars. Using `x@Either` (bare) leaves `a`, `b` as fresh inference variables.

**Nominal variants** use `[type [Tag1 body1] [Tag2 body2] ...]` multi-entry form — these are registered structurally by the type-checker, not evaluated as type-stage expressions.

```tinct
Result: [type [Ok a] [Err String]]     # nominal ADT — structural registration
Shape:  [type [circle: [radius: Int]]
              [rect:   [w: Int  h: Int]]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 30:1:7
  |
  1 | Result: [type [Ok a] [Err String]]     # nominal ADT — structural registration
    |       ^
```

**Type alias entries are excluded from record fields.** A `[type ...]` entry registers an alias in the type environment but contributes no field to the enclosing record's type. The evaluator returns an empty dict for type alias entries.

**Recursive type aliases** use two-pass registration: all aliases in a dict pre-register with `Type::Unknown` placeholder bodies (Pass 1), then resolve their actual bodies (Pass 2). Self-references resolve to `Type::Unknown` at the cycle boundary, breaking infinite expansion while preserving shallow access (up to MAX_ALIAS_DEPTH = 256 layers).

```tinct
List: [type [head: Int  tail: List]]    # recursive — two-pass resolves self-reference
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 31:1:5
  |
  1 | List: [type [head: Int  tail: List]]    # recursive — two-pass resolves self-reference
    |     ^
```

### 16. Type Dict Schema

Type-stage functions return type dicts. The canonical schema:

| `kind:` value | Fields | `Type::*` |
|---------------|--------|-----------|
| `"named"` | `name: String` | `Type::Int`, `Type::Str`, `Type::Bool`, `Type::Unknown`, named aliases |
| `"union"` | `members: [<type-dict> ...]` | `Type::Union(Vec<Type>)` |
| `"inter"` | `members: [<type-dict> ...]` | `Type::Intersection(Vec<Type>)` |
| `"seq"` | `element: <type-dict>` | `Type::Seq(Box<Type>)` |
| `"map"` | `key: <type-dict>  value: <type-dict>` | `Type::Map(Type, Type)` |
| `"fn"` | `return: <type-dict>  params: [<type-dict> ...]` | `Type::Function { ret, params }` |
| `"record"` | `fields: {name: <type-dict> ...}` | `Type::Record(Row)` |
| `"recursive"` | `var: String  body: <type-dict>` | `Type::Recursive { var, body }` (μ-types) |
| `"recvar"` | `name: String` | `Type::RecVar(String)` |
| `"type-stage-app"` | `fn: String  args: [<type-dict> ...]` | `Type::TypeStageApp { fn_name, args }` |

**Bare uppercase rule:** Unconstrained type positions use `Unknown`. Exception: `Fn` params use `Any` to express variadic-Any calling convention (any callable that accepts any argument types).

`dict_to_type()` errors on unknown `kind:` values or missing required fields — never silently produces `Unknown` for malformed dicts.

---

## Part V: Runtime Contracts

### 17. Default Values

`default:` on a parameter makes it optional. The default value is a thunk — lazily evaluated per call where the argument is absent.

```tinct
# Optional named parameter
fetch: [fn@String [url@String  timeout@[type: Int  default: 30]] ...]
[fetch "https://example.com"]            # timeout = 30
[fetch "https://example.com" timeout: 5] # timeout = 5

# In type assertion — fallback on type mismatch
port: [@[type: Int  default: 8080] config.port]  # 8080 if port absent or wrong type
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 32:2:6
  |
  2 | fetch: [fn@String [url@String  timeout@[type: Int  default: 30]] ...]
    |      ^
```

**`default:` does not interact with `is:`.** `default:` substitutes when the argument is **absent**. `is:` validates when the argument **is present**. A caller providing a value that fails `is:` gets a runtime error — the default is never used as a fallback for failed predicates.

`ast-of` on a parameter with `default:` returns the unevaluated AST expression for the default, not its evaluated value.

### 18. Runtime Predicates (`is:`)

`is:` attaches a runtime predicate to a parameter or assertion. The predicate receives the value and must return truthy for the annotation to pass.

**In function parameters (hard guard):**

```tinct
positive: [fn@Int [x@[type: Int  is: positive?]] ...]
# If caller passes x=(-1), predicate fails → runtime TypeError
# The default: value is NEVER used as fallback for is: failure
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 33:1:9
  |
  1 | positive: [fn@Int [x@[type: Int  is: positive?]] ...]
    |         ^
```

**In match arms (soft guard):**

```tinct
[match value
  n@[type: Int  is: positive?]: [str "positive: " n]   # falsy → skip to next arm
  n@Int:                         [str "non-positive: " n]]
=== error
type errors:
  undefined variable: value at 1:8-1:13

```

- `is:` in match arm: falsy → arm skipped, next arm tried (soft)
- `is:` in function parameter: falsy → runtime error (hard)
- `is:` throwing → always a hard runtime error, never a soft skip

**Type narrowing.** Recognized type predicates narrow the TypeVar's type in the arm body: `int?` → `Int`, `string?` → `String`, `dict?` → open record. User-defined predicates produce no narrowing — the type in the body is the matched value's pre-guard type.

**Narrowing only applies to AST control flow constructs.** Type narrowing fires only for `if`, `cond`, and `match` expressions (AST special forms with dedicated type checking rules). Prelude functions like `when` and `unless` (defined in `stdlib/prelude.llt` as calls to `builtin-if`) do NOT trigger narrowing because the type checker does not analyze function bodies — narrowing requires AST-level inspection. To narrow a value with a type predicate, use `if` directly: `[if [fn? x] ...]` instead of `[when [fn? x] ...]`.

**`is:` fires at materialization boundary** — when the value is first accessed, not at binding time.

### 19. Numeric Representation (`repr:`)

`repr:` constrains the integer bit width and signedness of a numeric parameter. Used for FFI, binary protocol parsing, and performance-sensitive numeric code.

```tinct
encode-byte: [fn@Null [value@[type: Int  repr: "u8"]] ...]
# repr: "u8" — type checker verifies value has a numeric type (Int, Float, or Number)
# runtime: no range checking — repr: is a static annotation, not a range validator
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 35:1:12
  |
  1 | encode-byte: [fn@Null [value@[type: Int  repr: "u8"]] ...]
    |            ^
```

Accepted `repr:` values: `"u8"`, `"i8"`, `"u16"`, `"i16"`, `"u32"`, `"i32"`, `"u64"`, `"i64"`.

`repr:` fires at materialization boundary alongside `is:`.

---

## Part VI: Advanced Features

### 20. Capability Types

Tinct uses capability types to statically track I/O permissions. Three capability types:

| Type | Meaning | Example binding |
|------|---------|-----------------|
| `DirCap` | Directory capability — filesystem access | `%pwd`, `%libdir`, user-declared |
| `NetCap` | Network capability — outbound connection allowlist | User-declared |
| `Handle` | File/stream handle — readable/writable I/O channel | Returned by `open`, `connect` |

```tinct
# Annotations
read-file: [fn@String [cap@DirCap  path@String]
  [slurp [open cap path "r"]]]

connect: [fn@Handle [nc@NetCap  host@String  port@Int] ...]

# caps: pragma on document header — declared required capabilities
--- caps: [%nc: @NetCap  %data: @DirCap]
[emit [connect %nc "api.example.com" 443]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 36:2:10
  |
  2 | read-file: [fn@String [cap@DirCap  path@String]
    |          ^
```

`DirCap`, `NetCap`, `Handle` are opaque base types — no parametric polymorphism. Subtyping is reflexive only (`DirCap <: DirCap`, all <: `Any`). `RevocableDirCap` matches `DirCap` at the type level (revocation is a runtime property).

The `%pwd`, `%libdir`, and `%stdin` capability variables are injected into the TypeEnv automatically; they do not need `caps:` declarations.

### 21. Recursive Type Aliases

Type aliases support self-reference via two-pass registration. All aliases in a dict pre-register with `Type::Unknown` placeholder bodies (Pass 1), then resolve actual bodies (Pass 2). Self-references resolve to `Type::Unknown` at the cycle boundary.

```tinct
List: [type [head: Int  tail: List]]
Tree: [type [value: Int  left: Tree  right: Tree]]

# Mutually recursive — must be in the same dict
A: [type [b_field: B]]
B: [type [a_field: A]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 37:1:5
  |
  1 | List: [type [head: Int  tail: List]]
    |     ^
```

**Depth limit.** Alias expansion is bounded at 256 layers (`MAX_ALIAS_DEPTH`). Exceeding this limit produces: `recursive type alias 'Name' exceeds maximum unfolding depth (256)`.

**Semantics: equi-recursive, not iso-recursive.** Aliases are transparent — they unfold automatically during type checking. There is no explicit `fold`/`unfold` syntax.

### 22. Literal Types

Integer and string literals carry their value in the type:

- `42` → `IntLiteral(42)` <: `Int` <: `Number`
- `"hello"` → `StringLiteral("hello")` <: `String`

Float and Bool literals do not have literal types: float equality is fragile (rounding, NaN), and Bool is trivially enumerable.

**Literal types enable computed key resolution.** When a dict has a computed key `[$k: expr]`, the type checker infers the type of `k`. If it resolves to a literal type (`StringLiteral("name")`), the field name is statically known:

```tinct
[k: "hello"  $k: 42]
# k : StringLiteral("hello") → field name "hello"
# type: [k: StringLiteral("hello")  hello: IntLiteral(42)]

[k: dynamic  $k: 42]
# k : String (not literal) → field "hello" excluded from Record type
# value 42 is still type-checked; field name is unknown statically
=== error
error: duplicate key "k"
 --> block 38:1:14
  |
  1 | [k: "hello"  $k: 42]
    |              ^^
```

Literal types widen only when an annotation demands the base type — they never widen implicitly.

---

## Part VII: Formal Specifications

### 23. Formal Grammar

```ebnf
(* Annotation attachment *)
param_annotation    = ${ "@" ~ annotation_value }
fn_annotation       = ${ "@" ~ annotation_value }
annotated_bare      = ${ bare_word ~ "@" ~ annotation_value }

(* Annotation value *)
annotation_value    = { simple_annotation | property_dict_annotation }
simple_annotation   = { identifier }
property_dict_annotation = { "[" ~ (annotation_entry ~ WHITESPACE*)* ~ "]" }
annotation_entry    = { (key ~ ":" ~ annotation_value) | annotation_value }

(* Type assertion expression *)
type_assert_body    = { "@" ~ annotation_value ~ value }

(* Chained parameterized annotation *)
chained_annotation  = ${ identifier ~ ("@" ~ annotation_value)+ }
```

`@` is `ImmediateAt` — emitted only when it appears directly after an identifier with no whitespace. This distinguishes `x@Int` (annotation) from `x @ Int` (which would be parsed differently if `@` were a regular operator).

### 24. Mixed-Stage Routing

Processing order within a `fn@[...]` metadata bracket (fixed; source key order is irrelevant):

| Step | Key | Action | Destination |
|------|-----|--------|-------------|
| 1 | `bind:` | Declare TypeVars as fresh vars | `ann_mapping` |
| 2 | `kinds:` | Register kind constraints | `kind_env` |
| 3 | `constraint:` keyed entries | Class constraints per entry | `state.constraints` |
| 4 | `constraint:` MPTC positional | Relate TypeVars via ClassEnv | `state.constraints` |
| 5 | `return:` / `type:` | Resolve as type-stage expression | Return type |
| 6 | `doc:` | Store documentation string | `TypeScheme.doc` |
| 7 | `default:`, `is:`, `repr:`, arbitrary | Deferred to runtime | Runtime metadata |

Mixed-stage routing for annotation brackets in general:

| Annotation form | Stage | Destination |
|-----------------|-------|-------------|
| `x@Int` | Type | `Type::Int` via `resolve_type_name` |
| `x@[or Int Null]` | Type-stage eval | `Type::Union` via type-stage Env |
| `x@[type: Int  default: 0]` | Split | `type:` → type-stage, `default:` → runtime |
| `fn@[bind: [a]  return: a  constraint: ...]` | Split | Per step table above |
| `[@Int expr]` | Type + runtime | Type assertion at materialization |
| `x@[is: pred]` in match | Runtime | Soft guard at match time |
| `x@[repr: "u8"]` | Runtime | Materialization boundary check |

### 25. Type Inference and Let-Generalization

Tinct uses Hindley-Milner inference with row polymorphism and levels-based let-generalization (Kiselyov 2013).

**TypeVar levels.** Each TypeVar carries an integer level (`TypeVar(String, u32)`) representing the nesting depth of its binding scope. Let-generalization generalizes TypeVars whose level exceeds the current enclosing level — preventing TypeVars from escaping their scope.

**Dict letrec inference.** Dict entries form a letrec scope. The type checker runs five passes:

1. **Pass 0:** Resolve key names (literal keys extracted; computed keys resolved via type inference in parent scope)
2. **Pass 1:** Bind all non-alias key names to fresh TypeVars at the current level
3. **Pass 2:** Register type aliases sequentially (each alias sees previously registered siblings)
4. **Pass 3:** Infer actual value types and unify with Pass 1 TypeVars
5. **Pass 4:** Generalize field types into polymorphic schemes

**Substitution idempotence invariant.** `Substitution::apply()` is idempotent — applying the same substitution twice yields the same result. Achieved by transitive chaining in `apply_inner()` (Robinson 1965).

**Alpha-equivalence.** Variable names are significant at the source level but irrelevant for principal types. `instantiate()` performs alpha-renaming (generating fresh `_t0`, `_t1`, ...) at each call site to prevent unintended unification between independent uses of the same polymorphic function.

**Constraint propagation.** When `bind(TypeVar(α), TypeVar(β))` occurs during unification, class constraints on `α` are transferred to `β` (deduplicated). This preserves the principal type property: the representative TypeVar accumulates all constraints from its equivalence class. `HasField` constraints are NOT transferred — they encode position-specific field structure.

See [Type Inference](06-type-inference.md) for the full let-generalization algorithm and constraint solving details.

### 26. Reflection and `ast-of`

`ast-of` on an annotated expression returns the resolved type as a type dict (via `type_to_dict()`) alongside the expression's AST structure:

```tinct
ast-of: [fn@a  min: [fn@[bind: [a]  return: a  constraint: [a: Comparable]] [xs@[Seq a]] ...]]
# → [kind: "fn"
#    return: [kind: "named" name: "a"]
#    params: [[kind: "seq" element: [kind: "named" name: "a"]]]
#    constraints: [[class: "Comparable" var: "a"]]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 39:1:7
  |
  1 | ast-of: [fn@a  min: [fn@[bind: [a]  return: a  constraint: [a: Comparable]] [xs@[Seq a]] ...]]
    |       ^
```

`ast-of` on a default: value returns the unevaluated AST expression dict — not the evaluated default value. This enables tooling to inspect the source expression of defaults without forcing evaluation.

The type dict schema used by `ast-of` matches the §16 Type Dict Schema exactly — `type_to_dict()` and `dict_to_type()` are inverse operations for all `Type::*` variants.

---

## Reference

- **Kiselyov, O. (2013).** "Efficient and Insightful Generalization." — [levels-based let-generalization; TypeVar level assignment]
- **Robinson, J.A. (1965).** "A Machine-Oriented Logic Based on the Resolution Principle." *JACM*, 12(1), 23–41. — [unification; substitution idempotence]
- **Gaster, B.R. & Jones, M.P. (1996).** "A Polymorphic Type System for Extensible Records and Variants." — [named parameters in calling convention; names not part of principal type]
- **Amadio, R.M. & Cardelli, L. (1993).** "Subtyping Recursive Types." *ACM TOPLAS*, 15(4). — [equi-recursive type equality; depth guard for decidability]
- **Jones, M.P. (1995).** *Qualified Types: Theory and Practice.* Cambridge. — [constraint schemes; processing order]
- **Jones, M.P. (2000).** "Type Classes with Functional Dependencies." *ESOP 2000*. — [fundep improvement; MPTC constraint propagation]

See [References](17-references.md) for complete citations.
