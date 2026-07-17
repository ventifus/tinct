# Type Annotations & Type Expressions

**Mandatory, bottom-up type inference with annotation-driven polymorphism, inspired by Hindley-Milner.** Every value has a type. Type errors raised early — good for LLMs and LSP feedback. Annotations are optional but enforced: write one and the compiler verifies it.

---

## Part I: Foundations

### 1. The `@` Concept

**`@` attaches a type or property dict** to a name, function, or expression. It is always a structural separator — not a valid identifier character. Wherever `@` appears immediately after a bare word (no whitespace), it separates the word from its annotation.

```tinct
x@Integer                       # parameter x has type Int
fn@String                   # function returns String
[@Integer expr]                 # type assertion: expr must be Int at runtime
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

### 1a. General Annotation Syntax

`@[...]` annotations (property dicts) attach to every significant grammar position, not only function parameters and return types. This enables uniform metadata — documentation, schema constraints, traversal roles, and user-defined fields — at every declaration point.

**Grammar positions where `@[...]` is valid:**

```tinct
# Top-level bindings (function or non-function)
my-fn@[doc: "..."  version: "1.0"  deprecated: false]: [fn ...]
Pi@[doc: "mathematical constant"]: 3.14159

# Type alias declarations
JsonValue@[doc: "..."  schema-id: "json-value"]: [type [or Int Float ...]]

# Constructor names in [type ...] declarations
TypeNode: [type
  [Union@[children: [fn [let u] u.types]  as-type: [fn [let u] u]  guarding: false]
    types: [Seq TypeNode]]
  ...]

# Record/dict field type declarations
Config: [type [record
  host@[required: true  doc: "hostname"]:         String
  port@[default: 8080   doc: "port number"]:       Int
  timeout@[default: 30  doc: "seconds"]:           Int]]

# [class ...], [instance ...], [macro ...], [let ...] declarations
[class@[doc: "structural children of a TypeNode"] [TypeNodeChildren t]
  [fn@[Seq TypeNode] [children [let _@t]]]]

[macro@[doc: "..."  inject: it: default-expr] my-macro [let pattern] body]

[let x@[doc: "intermediate result"] [+ a b]]
```

**Storage model.** All annotations are stored as `IndexMap<String, Value>` — a uniform open dict. There are three storage sites:

- **`Value::Function` values**: `FnAnnotation.extra: IndexMap<String, Value>` alongside existing `doc`, `return_type`, and params fields. All annotation fields — well-known and custom — are stored here uniformly; `annotation-of` reads `extra` as the canonical annotation dict.
- **All other values** (`Value::String`, `Value::Int`, `Value::Dict`, etc.): `Value::Annotated { inner: ThunkId, annotation: Box<Value> }` wraps any non-function value with its annotation dict. All other Value operations unwrap `Annotated` transparently (T-1123 tracks making Display/Debug/to_tinct/value_to_json transparent).
- **Type-level positions** (type alias declarations, record field type annotations): `TyConDef.annotation: IndexMap<String, Value>` for type-level positions. Record field type annotations are stored in `TypeNode.Dict.field_annotations`. _(Current state: `TyConDef.annotation` and `field_annotations` fields exist but are always `None`/empty until T-1122 (eval_type_stage_expr evaluation) lands.)_

**`annotation-of` is a Rust builtin** that reads from all three storage sites uniformly, returning the annotation dict or an empty dict when no annotation is present. It is available at both runtime and in the type-stage evaluator.

**No fixed or privileged fields.** The distinction between well-known keys (`doc:`, `as-type:`, `guarding:`, `@Child`) and user-defined keys is purely a matter of which code reads which keys — there is no architectural distinction. Any code can read any key via `annotation-of`.

### 2. Simple Type Annotations

`x@Type` declares the compile-time type of parameter `x`. If the annotation is a bare name, it is a type reference (uppercase = concrete, lowercase = TypeVar). If it is a bracket expression, it is resolved as a type-stage expression.

```tinct
x@Integer                       # x has type Int
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

- Uppercase first letter: concrete types (`Int`, `String`, `Bool`, `Null`, `Top`)
- Lowercase first letter: type variables (`a`, `b`, `k`, `v`)
- `String` / `Str`: `String` is the user-facing annotation name; `Str` is the internal name used in error messages (the corresponding TypeNode is `TypeNode.String`). Use `String` in annotations.
- `Null`: the empty record `[]` — closed record with no fields. Use `fn@Null` for functions returning no meaningful value.
- `Top`: universal supertype (`⊤`) — every type is a subtype of `Top`. Accepts any value in the subtyping sense.
- `Unknown`: gradual type (`?`) — any type is consistent with `Unknown` for gradual typing, but it does NOT imply "accepts any value" in the subtyping sense. Unconstrained inference positions resolve to `Unknown`.

### 3. Type Assertions

**`[@Type expr]` asserts the type of an expression at runtime.** Materializes the value, checks its type, and throws `TypeError` on mismatch.

```tinct
data: [from-json input]           # type: Any

name: [@String data.name]         # throws if data.name is not a String
port: [@Integer    data.port]         # throws if data.port is not an Int

# Inline
[+ [@Integer x] 1]

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
xs@[Seq Int]            # Seq of Int
scores@[Map String Int] # Map from String to Int
nested@[Seq [Seq Int]]  # Seq of Seq of Int
pair@[Pair Int String]  # Pair of Int and String (user-defined)
=== error
type errors:
  expected record type, got Seq[Int] at 1:1-1:13
  expected record type, got Map[String Int] at 2:1-2:24
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
fn@[return: [or Int Null]] [xs@[Seq Int]  target@Integer] ...]
=== error
error: unmatched closing bracket
 --> block 8:3:58
  |
  3 | fn@[return: [or Int Null]] [xs@[Seq Int]  target@Integer] ...]
    |                                                          ^
```

`or` is a type-stage function in the prelude. It produces `TypeNode.Union { types: [...] }`. Union members are normalized: deduplicated, sorted, and flattened (nested unions collapse).

**Intersection — `each` type-stage combinator:**

```tinct
x@[each Comparable Printable]       # Comparable ∩ Printable
constraint: [a: [each Comparable Printable]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 9:2:11
  |
  2 | constraint: [a: [each Comparable Printable]]
    |           ^
```

`each` produces `TypeNode.Intersect { types: [...] }`. In `constraint:` position, each member becomes a separate `Constraint::Class`. In annotation position, it produces a `TypeNode.Intersect` value.

**BAS annotation call-form — `@[[all A B]]` and `@[[without A]]`:**

The `all` and `without` type-stage functions are also available in the double-bracket annotation form for inline BAS types:

```tinct
x@[[all Comparable Printable]]    # intersection: Comparable ∩ Printable
x@[[without String]]             # negation: ~String (any type except String)
```

`@[[all T1 T2 ...]]` — the inner `[all T1 T2]` parses as `Expr::Call { func: VarRef("all"), args: [T1, T2, ...], implied: true }`. `eval_type_stage_expr` evaluates `all` as an ordinary type-stage function, producing `TypeNode.Intersect { types: [T1, T2, ...] }` (normalized: deduplicated, sorted, flattened).

`@[[without T]]` — the inner `[without T]` likewise parses as a Call with head `without`, producing a negation TypeNode value.

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
count-keys: [fn@Integer [d@Dict] [length [keys d]]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 12:1:11
  |
  1 | count-keys: [fn@Integer [d@Dict] [length [keys d]]]
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
[fn@Integer [x@Integer y@Integer] [+ x y]]

# Type:         Fn@Return [ParamTypes]
[Fn@Integer [Int Int]]
=== error
type errors:
  expected record type, got Fn@Integer [x: Int y: Int] at 2:1-2:31
  undefined variable: Int at 5:10-5:13

```

```tinct
[Fn@b [a]]                    # function from a to b
[Fn@Boolean [a]]                 # predicate
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

**Subtyping:** named function types are subtypes of anonymous ones by dropping parameter names (Gaster & Jones 1996). `Fn@Integer [x: Int  y: Int]` <: `Fn@Integer [Int Int]`.

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
transform: [fn@[bind: [a b]  return: [Seq b]  constraint: [a: Printable]]
             [xs@[Seq a]  f@[Fn@b [a]]] ...]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 17:2:10
  |
  2 | transform: [fn@[bind: [a b]  return: [Seq b]  constraint: [a: Printable]]
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
constraint: [a: Comparable  b: Printable]

# Multiple constraints on one TypeVar — each combinator
constraint: [a: [each Comparable Printable]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 19:2:11
  |
  2 | constraint: [a: Comparable]
    |           ^
```

**Routing:** constraint values are type-stage expressions. `Comparable` resolves to `TypeNode.TypeConstructor { name: "Comparable" }` → `Constraint::Class("Comparable", α)`. `[each Comparable Printable]` resolves to `TypeNode.Intersect { types: [...] }` → two separate `Constraint::Class` entries.

**Interaction with inference.** Explicit constraints compose with inferred constraints. If `constraint: [a: Comparable]` is declared and the body also uses `a` in an `Equatable` context, both register; constraint simplification removes `Equatable a` since `Comparable` entails it via the superclass relation.

**`doc:` and LSP hover.** The doc string is stored in `TypeScheme.doc` and displayed below the inferred signature in LSP hover:

```text
min: Comparable a => Fn@a [[Seq a]]
Return smallest element
```

**Examples:**

```tinct
min: [fn@[bind: [a]  return: a  constraint: [a: Comparable]  doc: "Return smallest element"]
      [xs@[Seq a]] ...]
# Inferred: Comparable a => Fn@a [[Seq a]]

compare: [fn@[bind: [a b]  return: Bool  constraint: [a: Comparable  b: Printable]]
          [x@a  y@a  logger@b] ...]
# Inferred: (Comparable a, Printable b) => Fn@Boolean [a a b]

display-sorted: [fn@[bind: [a]  return: String  constraint: [a: [each Comparable Printable]]]
                 [xs@[Seq a]] ...]
# Inferred: (Comparable a, Printable a) => Fn@String [[Seq a]]

check-all: [fn@[bind: [a]  return: Bool  constraint: [a: Equatable]]
            [xs@[Seq a]  target@a] ...]
# Inferred: Equatable a => Fn@Boolean [[Seq a] a]

between: [fn@[bind: [a]
              return: [Fn@Boolean [a]]
              constraint: [a: Comparable]
              doc: "Return a predicate for [lo, hi)"]
          [lo@a  hi@a]
          [fn@Boolean [x@a] [and [>= x lo] [< x hi]]]]
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

**Recursive type-stage functions are not supported.** The lazy evaluator defers self-calls as thunks; the annotation resolver forces thunks during traversal, causing infinite unrolling. For recursive type structures, use named aliases (the expansion stack detects self-references automatically and produces `TypeNode.Recursive`) or the `mu` combinator for inline annotations.

### 13. Type Prelude and Type Constructors

All types — including builtins — are declared in prelude using the unified `[type ...]` syntax. This makes `TyConEnv` the complete and authoritative registry of all types and their runtime representations.

**Builtin type declarations** in prelude use `[builtin-type "X"]` to associate a type name with its Rust-level discriminant:

```tinct
--- stage: type
Int:    [type [builtin-type "Int"]]     # Value::Int
Str:    [type [builtin-type "Str"]]     # Value::String
Bool:   [type [builtin-type "Bool"]]    # Value::Bool
Float:  [type [builtin-type "Float"]]   # Value::Float
Bytes:  [type [builtin-type "Bytes"]]   # Value::Bytes
Dict:   [type [builtin-type "Dict"]]    # Value::Dict
Fn:     [type [builtin-type "Fn"]]      # Value::Function | Value::Builtin
Handle: [type [builtin-type "Handle"]]  # Value::Handle

Number: [type [or Int Float]]           # transparent alias — union of numeric primitives
```

**Nominal and structural type declarations** in prelude:

```tinct
--- stage: type
Variance: [type Covariant Contravariant Invariant Phantom]
Absent:   [type Absent]
Seq:      [type [let a@Covariant]  Nil  [Cons head: a  tail: [Seq a]]]
Map:      [type [let k@Equatable v]  [_@k : v]]
```

`Seq` is a nominal ADT whose constructors are `Seq.End` (unit terminal) and `Seq.Cons` (payload with `head` and `tail` fields). `Map K V` is a transparent alias for the uniform column constraint `{_@K : V}` — any dict whose values are all of type `V` and whose keys satisfy `K`. `Absent` is a unit nominal type for expressing first-class absence.

**Name resolution order** in the type environment:

All named types — primitive builtins, structural aliases, and nominal ADTs — are registered as `Arc<TyConDef>` entries in a single unified store (`HashMap<String, Arc<TyConDef>>`). There is no separate type-alias table. `expand_named` performs a single lookup; all names go through the same path. Primitive TypeNode constructors (`Int`, `Float`, `Bool`, `Absent`, etc.) are registered as `TyConDef` entries with `params: []` and a pre-interned `TypeNode` body value.

**Name resolution order** in type-stage Env:

1. Type-stage bindings (`or`, `each`, `mu`, user-defined combinators)
2. Prelude `TypeNode` ADT constructors (`TypeNode.Int`, `TypeNode.Union`, etc.)
3. All named types registered as `Arc<TyConDef>` (primitives and user-declared)

### 14. Annotation Resolution

Annotation brackets `@[...]` are resolved by evaluating their contents in the type-stage Env via `eval_type_stage_expr`, then normalizing the result through `TypeNode.as-type` and `expand_all_tycon_apps`. The resolver produces fully normalized `Type` values — no `TypeNode.TypeApplication` or bare `TypeNode.TypeConstructor` references remain after resolution. The type checker receives only concrete forms: primitives, Record, Union, Intersect, Arrow, Recursive, RecursiveRef, TypeVar, and qualified TypeConstructor leaves.

Note: A `CheckerType` newtype wrapping `TypeNode` values is a planned future refactor (see `doc/whatif/equirecursive-types.md`) but is not yet implemented. The current type checker uses the `Type` enum from `src/type_def.rs` directly. Annotation resolution produces `Type` values via `typenode_value_to_type` — the bridge between tinct-level `TypeNode` values and the `Type` enum.

```text
# Named annotation path (@Integer, @ListA, @Color, @Maybe Int):
expand_named(name, args, expansion_stack)
  → TypeNode value (normalized, may be TypeNode.Recursive for self-referential aliases)
  → typenode_value_to_type(...)
  → Type (concrete form used by the type checker)

# Expression annotation path (@[or Int Null], @[mu [fn [let self] ...]], @[user-fn args]):
eval_type_stage_expr(expr, type_stage_env)          # evaluate as ordinary tinct code
  → typenode_normalize(result, env.as_type_fn)       # TypeNode.as-type dispatch
  → expand_all_tycon_apps(normalized, expansion_stack) # eliminate TypeApplication/bare TypeConstructor
  → typenode_value_to_type(...)                       # bridge to Type enum
  → Type (fully normalized, concrete form)
```

Type-stage expressions **actually execute**: users define type-stage functions in `--- stage: type` sections and the resolver calls them. There are no hardcoded special cases for `or`, `record`, `arrow`, or `mu` in the resolver — all are ordinary type-stage functions returning `TypeNode` values.

**Disambiguation of bracket annotation contents:**

- Any keyed entry matching a metadata key (`bind:`, `return:`, `constraint:`, `kinds:`, `doc:`) → metadata dict (not a type expression)
- All-keyed with unrecognized keys → record type
- Mixed positional and keyed with recognized metadata keys → type error
- `or:` (colon-suffixed) → always a dict key, always a record field name
- `or` (bare in head position) → always a type-stage function call

```tinct
@[or: Int  port: Int]    # Record schema: fields "or" and "port"
@[or Int Null]           # Union type: Int | Null → TypeNode.Union [TypeNode.Int  TypeNode.Absent]
=== error
error: @ annotations outside type-assert or param contexts not yet supported
 --> block 28:1:1
  |
  1 | @[or: Int  port: Int]    # Record schema: fields "or" and "port"
    | ^
```

**`TypeStageApp`** — when annotation brackets contain TypeVar arguments that are not yet ground, the resolver produces a lazy `TypeStageApp` node instead of evaluating eagerly. It reduces to a concrete type when the TypeVars are resolved during inference.

### 15. Type Alias Declarations

`[type ...]` declares a named type alias or nominal ADT. Four rules govern all `[type ...]` forms:

1. **`[let ...]` is the only way to introduce type parameters** — same as `[fn [let x y] body]`. Without `[let ...]`, lowercase names in type bodies resolve from the enclosing scope as existing types; they are not created as new TypeVars.
2. **Unit constructors are bare uppercase words.** `Red`, `SIGTERM`, `Nil` — no brackets.
3. **`builtin-type` bodies** mark a type as Rust-backed with a runtime discriminant string.
4. **Dict-entry form is the binding form.** `Name: [type ...]` — the name is the dict key.

**Five visually distinct forms:**

| Body content | Kind | Nominal? |
|---|---|---|
| Structural type expression | transparent alias | no |
| `[let ...]` + structural body | parameterized transparent alias | no |
| Uppercase bare words and `Name: [fields]` keyed entries | nominal ADT | yes |
| `[let ...]` + constructors | parameterized nominal ADT | yes |
| `[builtin-type "X"]` | opaque, Rust-backed | yes |

Three forms in the nominal ADT body:
- **Unit constructor** — bare uppercase word: `Red`, `None`, `Noop`
- **Payload constructor** — keyed entry: `Ok: [value: a]`, `Circle: [r: Int]`
- (Unit constructors with bracket form `[Red]` are no longer valid; use bare words)

```tinct
# Transparent aliases
NullableInt: [type [or Int Null]]
Name:        [type String]
Pair:        [type [let a b]  [first: a  second: b]]

# Nominal ADTs — constructors accessed via dot
Signal: [type SIGTERM SIGINT SIGHUP]
Result: [type [let a]  Ok: [value: a]  Error: [msg: String]]
Maybe:  [type [let a]  Some: [value: a]  None]
Color:  [type Red Green Blue]

# Parameterized with variance annotations
Tree:   [type [let a@Covariant]  Leaf  Node: [value: a  left: [Tree a]  right: [Tree a]]]

# Builtin-type declarations (Rust Value variants)
Int:    [type [builtin-type "Int"]]
Handle: [type [let a] [builtin-type "Handle"]]
```

**Variance annotations** on type parameters use `name@VarianceName`:

| Annotation | Meaning |
|---|---|
| `a@Covariant` | `F a <: F b` when `a <: b` — producer/container position |
| `a@Contravariant` | `F a <: F b` when `b <: a` — consumer/handler position |
| `a` (none) | invariant — default; safe for opaque types |
| `a@Phantom` | `F a <: F b` always — `a` is type-level only, no runtime presence |

For transparent aliases, the compiler infers variance via polarity analysis (Dolan 2017 §4). Explicit annotations serve as checked declarations — a mismatch is a type error. Variance parsing and polarity analysis are implemented (S-842/S-843). Variance-directed subtyping for user-declared types (e.g. `Tree Int <: Tree Number` via `@Covariant`) is infrastructure-complete; cross-scope TyCon identity enforcement is pending B-343.

**Parameterized alias use:** `x@[Either Int String]` substitutes `a=Int`, `b=String` directly — it is a substitution, not instantiation of fresh TypeVars. Using `x@Either` (bare) leaves `a`, `b` as fresh inference variables.

**Constructor access and patterns.** A type declaration creates both a type in the type system and a dict value whose fields are the constructors. The only binding created is the type name:

```tinct
Color: [type Red Green Blue]
# value: Color = {Red: <Color.Red variant>, Green: ..., Blue: ...}
c: Color.Red          # dot access to the constructor
Result.Ok             # payload constructor function
Net.Transport.Tcp     # multi-level
```

Constructor patterns use the same dot syntax in pattern head position:

```tinct
[match sig
  Signal.SIGTERM: [cleanup]
  Signal.SIGINT:  [interrupt]]

[match xs
  Seq.End:       "empty"
  [Seq.Cons c]:  c.head]

[match result
  [Result.Ok v]:    v.value
  [Result.Error e]: [log e]]
```

Dot-access in pattern head position is syntactically assembled by the parser via `flatten_dot_access_to_tag` in `src/ast.rs`. Bare uppercase words in constructor patterns are also recognized and rewritten to their qualified form by `typecheck_match.rs`.

**Constructor injection in expression position** — bare `Ok`, `Error`, `Some`, `None` work in expression position because `inject_adt_constructors_expr` (in `src/desugar.rs`) rewrites each `[type ...]` declaration to also inject its constructors as sibling dict entries. For example, declaring `Result: [type Ok: [value: a] Error: [msg: String]]` causes `Ok` and `Error` to be injected as callable constructor functions alongside `Result` in the enclosing scope. No separate prelude aliases are needed — and none exist (adding them would cause E030 duplicate key). B-340 tracks the type-checker UX limitation that these injected names are not currently visible to the type checker, which produces spurious T002 "undefined variable" warnings for bare `Ok`/`Error`/`Some`/`None` in user code.

Pattern position requires qualified forms: `[Result.Ok v]:`, `[Maybe.None]:`, etc.

**Type alias entries are excluded from record fields.** A `[type ...]` entry registers in the type environment but contributes no field to the enclosing record's type.

**Recursive type aliases** are resolved via the expansion stack. All aliases in a dict pre-register as `Arc<TyConDef>` entries (Pass 1); the resolver then expands each alias body on demand (Pass 2). When a self-reference is encountered during expansion, `Arc::ptr_eq` detects the cycle, and a `TypeNode.RecursiveRef` sentinel is emitted — the result is wrapped in `TypeNode.Recursive` at the cycle-origin level. No `Type::Unknown` placeholder is used; the result is a proper μ-type. See §TypeNode: The Primary Type Representation for the full expansion algorithm.

```tinct
List: [type [head: Int  tail: List]]    # recursive — expansion stack emits TypeNode.Recursive automatically
```

### 15a. Column Constraints — Uniform Row Types

A **column constraint** declares that all present fields in a dict satisfy a value type constraint, expressed as `{_ : V}` (value type only) or `{_@K : V}` (key type and value type). These appear in annotation position alongside named fields:

```tinct
config@{_ : String}               # all values String
counts@{_ : Int}                  # all values Int
mixed@{host: String  _ : Int}     # host is String; all other fields are Int
data@{_@String : Int}             # String keys, Int values
```

`Map K V` in prelude is defined as `[type [let k@Equatable v] [_@k : v]]` — a uniform dict where all values have type `V` and all keys satisfy `K`. So `@[Map String Int]` and `@{_@String : Int}` are equivalent after alias expansion.

**Subtyping rules:**

- `{f1: T1, ..., fn: Tn, tail: Empty} <: {_ : V}` when all `Ti <: V`
- `{_ : V1} <: {_ : V2}` when `V1 <: V2` (covariant in value type)
- `{_@K1 : V1} <: {_@K2 : V2}` when `K1 <: K2` and `V1 <: V2`
- `{_@K : V} <: {_ : V}` always (keyed constraint is a subtype of unkeyed)

**Runtime:** uniform row matching wraps each field access in a guard thunk checked on demand, preserving lazy evaluation.

### 15b. Absent — First-Class Absence

`Absent` is a unit nominal type that separates "not present" from `[]` (empty collection):

```tinct
Absent: [type Absent]
absent?: [fn@Boolean [let x@Unknown] [match x Absent.Absent: true  _: false]]
```

`[or Absent T]` is the structural optional type. Pattern matching is the canonical narrowing form:

```tinct
[match x
  Absent.Absent:  "missing"
  _:              "present"]
```

Builtins that return a missing value (`get?`, `env`) return `Absent.Absent` rather than `[]`. Testing with `absent?` or pattern matching on `Absent.Absent` is correct; `null?` checks only for `[]` (empty collection) and is not interchangeable. Note: `head` raises on `Seq.End` rather than returning `Absent.Absent`. `get-in?` returns `Absent.Absent` if any key in the path is missing; signature: `Dict -> [Seq String] -> Any`. Example: `[get-in? dict ["a" "b" "c"]]` (implemented in the prelude, T-1047, S-855).

### 16. TypeNode: The Type-Stage Value Representation

`TypeNode` is the tinct-level value representation of types — produced by the type-stage evaluator and used by type-stage expressions. The type checker's internal representation is the `Type` Rust enum (defined in `src/type_def.rs`). The bridge between the two is `typenode_value_to_type`, which converts a `TypeNode` tinct value to a `Type` enum value for use by the type checker.

Note: A `CheckerType` newtype wrapping `TypeNode` values as the type checker's primary representation is a planned future refactor (see `doc/whatif/equirecursive-types.md`) but is not yet implemented. Until then, `Type` is the authoritative representation inside the type checker. See [Type Inference](06-type-inference.md) for the canonical description of the `Type` enum and its role.

**TypeVar in type-stage.** `TypeNode.TypeVar { name: String  level: Int }` is the tinct-level representation of an inference variable. After conversion via `typenode_value_to_type`, it becomes `Type::TypeVar(name, level)` inside the type checker. `walk_type` finds TypeVars automatically via `TypeNode.children`; only four walkers (`is_subtype_bas` / `is_atom_subtype`, `unify`, `Substitution::apply`, `PartialEq`) require explicit Rust arms for TypeVar.

**TypeNode constructors** (all declared in the prelude `--- stage: type` section):

| Constructor | Fields | Role |
|-------------|--------|------|
| `TypeNode.Int`, `.Float`, `.String`, `.Bool`, `.Absent`, `.Unknown`, `.Never` | — | Primitive leaf |
| `TypeNode.Dict` | `fields@Child: [Map String TypeNode]`, `open: Bool` | Structural dict |
| `TypeNode.Union` | `types@Child: [Seq TypeNode]` | Union (BAS ∨) |
| `TypeNode.Intersect` | `types@Child: [Seq TypeNode]` | Intersection (BAS ∧) |
| `TypeNode.Arrow` | `params@Child: [Seq TypeNode]`, `result@Child: TypeNode` | Function type |
| `TypeNode.TypeConstructor` | `name: String` | Transient (bare name) or leaf identity (qualified, e.g. `"Color.Red"`) |
| `TypeNode.TypeApplication` | `ctor@Child: TypeNode`, `args@Child: [Seq TypeNode]` | Always transient — eliminated by normalization |
| `TypeNode.Recursive` | `var: String`, `body@Child: TypeNode` | μ-type (equirecursive) |
| `TypeNode.RecursiveRef` | `name: String` | Self-reference sentinel inside a Recursive body |
| `TypeNode.TypeVar` | `name: String`, `level: Int` | Inference variable (Kiselyov 2013 creation-time level) |

`TypeNode.TypeApplication` and bare (unqualified) `TypeNode.TypeConstructor` are transient — they exist during type-stage computation but are always eliminated by `expand_all_tycon_apps` before the type checker sees the result. After normalization, the type checker works only with the non-transient forms.

**`TypeNode-ctor t`** returns the constructor function for a TypeNode value: `[get TypeNode [last [str-split "." [tag-of t]]]]`. This expression appears inline in the traversal protocol functions.

**The `@Child` field annotation** marks a field in a `[type ...]` declaration as containing TypeNode children. The traversal role is inferred from the declared field type:

| Declared field type | Traversal role | Effect in `map-children` |
|--------------------|---------------|--------------------------|
| `TypeNode` | One | Apply `f` to the single value |
| `[Seq TypeNode]` | Seq | Apply `f` to each element |
| `[Map K TypeNode]` | MapValues | Apply `f` to each map value, preserve keys |

Fields without `@Child` are non-children and pass through unchanged. The `@Child` annotation is stored in `TyConDef.field_annotations` at desugar time.

**`TypeNode.children` and `TypeNode.map-children`** are derived generically from `@Child` field annotations — no per-constructor implementation is required:

```tinct
# children: collect all @Child fields into a flat Seq
TypeNode.children: [fn [let t]
  [flat-map [child-fields [TypeNode-ctor t]] [fn [let field]
    [let val [get t field]]
    [match [child-role [TypeNode-ctor t] field]
      One:       [Seq val]
      Seq:       val
      MapValues: [values val]]]]]

# map-children: apply f to each @Child field, reconstruct same-shaped variant
TypeNode.map-children: [fn [let f t]
  [variant [tag-of t]
    [object-map [fields [TypeNode-ctor t]] [fn [let field val]
      [if [child-field? [TypeNode-ctor t] field]
        [match [child-role [TypeNode-ctor t] field]
          One:       [f val]
          Seq:       [map f val]
          MapValues: [map-values f val]]
        val]]]]]

# as-type: normalize user-defined constructors to existing forms
TypeNode.as-type: [fn [let t]
  [let ann [annotation-of [TypeNode-ctor t]]]
  [if [has? ann as-type] [[get ann as-type] t] t]]
```

Helper functions (`child-fields`, `child-role`, `child-field?`) read the unified annotation dict produced from `@Child` processing. **Note:** retrieval of `field-annotations` via `annotation-of` on a constructor function requires T-1124 (support for expression-valued annotation fields in `extract_fn_annotation_extra`). Until T-1124 lands, `@Child` field roles are stored in `ChildFieldAnnotation` structs in the `AliasConstructor` at desugar time but are not materialized into `FnAnnotation.extra` because the `field-annotations:` entry value is a dict (an expression, not a scalar literal), which `extract_fn_annotation_extra` currently silently drops.

**Adding a new TypeNode constructor** requires: (1) declare it with `@[as-type: fn  guarding: Bool]` on the constructor name and `@Child` on TypeNode-typed fields; (2) add explicit arms only to `is_atom_subtype` (in `src/bas.rs`, called via `is_subtype_bas`), `unify`, `Substitution::apply`, and `PartialEq` for the new constructor's semantics. All traversal walkers (`walk_type`, `collect_type_vars`, `has_inference_vars`, etc.) pick up the new constructor automatically via `TypeNode.children` — no Rust changes needed.

**Inline recursive types via `mu`.** The `mu` prelude combinator creates `TypeNode.Recursive` values inline without a named alias:

```tinct
# Named alias form (expansion stack handles the self-reference automatically)
IntList: [type [or Absent [record head: Int  tail: IntList]]]

# Inline mu form — equivalent, no alias needed
depth: [fn@Integer [tree@[mu [fn [let self] [or Absent [record value: Int  left: self  right: self]]]]]]
  [if [absent? tree] 0 [+ 1 [max [depth tree.left] [depth tree.right]]]]
```

`mu` generates the binder var via `gensym-with-scope`, calls the body function eagerly, and stores the concrete TypeNode result — no deferred function is stored in `Recursive.body`. Contractiveness is checked at construction time: `μa.a` and `μa.(a | Int)` are rejected with `TypeError(NonContractive)`.

**Coinductive subtype checking (S-Exp + S-Assum).** `is_subtype_bas` (in `src/type_def.rs`) delegates to RDNF-based `is_atom_subtype` (in `src/bas.rs`). Sigma (`&mut HashSet<(String, String)>`) is threaded through all arms. When both sides are `TypeNode.Recursive`, the pair `(a.var, b.var)` is inserted into sigma (S-Assum) before proceeding. S-Exp unfolds a `Recursive` via `unfold_once` — substituting all `RecursiveRef(var)` occurrences with the full `Recursive` node — and re-enters. Sigma prevents divergence: when the same pair appears again, the hypothesis fires immediately and returns `true`. This approach is proven sound for BAS (Chau & Parreaux 2026).

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
positive: [fn@Integer [x@[type: Int  is: positive?]] ...]
# If caller passes x=(-1), predicate fails → runtime TypeError
# The default: value is NEVER used as fallback for is: failure
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 33:1:9
  |
  1 | positive: [fn@Integer [x@[type: Int  is: positive?]] ...]
    |         ^
```

**In match arms (soft guard):**

```tinct
[match value
  n@[type: Int  is: positive?]: [str "positive: " n]   # falsy → skip to next arm
  n@Integer:                         [str "non-positive: " n]]
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
| `DirCap` | Directory capability — filesystem access | `%cwd`, `%libdir`, user-declared |
| `NetCap` | Network capability — outbound connection allowlist | User-declared |
| `Handle` | File/stream handle — readable/writable I/O channel | Returned by `open`, `connect` |

```tinct
# Annotations
read-file: [fn@String [cap@DirCap  path@String]
  [slurp [open cap path Readable]]]

connect: [fn@Handle [nc@NetCap  host@String  port@Integer] ...]

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

`DirCap` and `NetCap` are opaque base types — subtyping is reflexive only (`DirCap <: DirCap`, all <: `Any`). `RevocableDirCap` matches `DirCap` at the type level (revocation is a runtime property).

`Handle` is parameterized with a capability row describing the handle's properties. The notation is `Handle[Readable Writable]` for a handle with both read and write capabilities. The inner type is a Row of capability tags:

```tinct
Handle[Readable Writable]           # File handle with read and write access
Handle[Readable Stream]             # Stream handle with read access
Handle[Writable Appendable Binary]  # Binary file handle, append mode
```

Capability tags registered in TypeEnv: `Readable`, `Writable`, `Appendable`, `Binary`, `Seekable`, `Stream`, `Tls`, `Text`, `Exclusive`, `Sync`, `NoFollow`.

Subtyping is covariant in the capability row: `Handle[Readable Writable] <: Handle[Readable]` (more capabilities satisfy fewer). `TypeNode.Unknown` as the inner type represents unknown capabilities (gradual typing fallback).

The `%cwd`, `%libdir`, and `%stdin` capability variables are injected into the TypeEnv automatically; they do not need `caps:` declarations.

### 21. Recursive Type Aliases

Recursive type aliases are resolved via the expansion stack. All aliases in a dict pre-register as `Arc<TyConDef>` entries (Pass 1). During body expansion (Pass 2), `expand_named` uses `Arc::ptr_eq` to detect self-references in the expansion stack: when a self-reference is encountered, `TypeNode.RecursiveRef` is emitted for that position, and the result is wrapped in `TypeNode.Recursive` at the cycle-origin level. No placeholder body is used at the cycle boundary — the result is a proper equirecursive μ-type.

```tinct
List: [type [head: Int  tail: List]]
Tree: [type [value: Int  left: Tree  right: Tree]]

# Mutually recursive — must be in the same dict; expansion stack handles the cycle
A: [type [b_field: B]]
B: [type [a_field: A]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 37:1:5
  |
  1 | List: [type [head: Int  tail: List]]
    |     ^
```

**Mutually recursive aliases.** `expand_named` handles mutual recursion by pre-assigning fresh binder names to all entries on the expansion stack before expanding. Only the alias at the cycle origin is wrapped in `TypeNode.Recursive`; intermediate aliases inline their bodies into the result. The wrapping rule: wrap `TypeNode.Recursive` only when popping the stack entry whose fresh name appears in the expanded body.

**Semantics: equirecursive, not isorecursive.** Aliases are transparent — they unfold automatically during type checking via S-Exp. There is no explicit `fold`/`unfold` syntax. `TypeNode.Recursive` and its unfolded form are equal by the coinductive S-Assum rule.

### 22. Gradual Typing Boundaries

**`TypeNode.Unknown` disables static typing for every expression it touches.** Because consistency (`~`) is non-transitive and symmetric, `Unknown` propagates silently: `Int ~ Unknown` and `Unknown ~ Str`, so an `Int` can flow into a `Str` context through an `Unknown` intermediary without a type error.

**Unknown is a last resort** for values whose type is genuinely opaque at compile time (e.g., untyped `include` files, FFI boundaries with no schema, builtin returns that cannot be precisely typed). Misuse makes the type checker useless.

**Boundary leakage: Unknown-typed bindings at document boundaries.**

When a top-level binding in a document has type `Unknown`, it propagates across document boundaries via `include`:

```tinct
--- file: lib.llt
untyped-value: [from-json "[1,2,3]"]  # Type: Unknown (no annotation)

--- file: main.llt
[include %libdir "lib.llt"]
result: [+ untyped-value 10]          # Type error silently bypassed
```

The value `untyped-value` has type `Unknown` because `from-json` returns `Unknown` when no annotation is provided. When `main.llt` includes `lib.llt`, the `Unknown` type leaks across the boundary. The expression `[+ untyped-value 10]` should fail (adding a Seq to an Int), but the `Unknown` type makes the checker pass it.

**Mitigation strategies:**

1. **Annotate public exports** — add type annotations to all bindings exported from library documents:

   ```tinct
   values@[Seq Int]: [from-json "[1,2,3]"]
   ```

2. **Use TypeAssert at boundaries** — wrap `Unknown`-typed expressions in `[@Type expr]` to enforce a runtime contract and refine the type for downstream code:

   ```tinct
   values: [@[Seq Any] [from-json "[1,2,3]"]]  # Runtime check + static refinement
   ```

3. **Audit Unknown sources** — search for `TypeNode.Unknown` in builtin type signatures and stdlib code. Replace with precise types (unions, TypeVars, `Top`) wherever possible.

**Lint opportunity (future work):** Emit a T002 advisory when a top-level binding in a document (not a letrec dict entry) has type `Unknown` and is not wrapped in a TypeAssert or annotated explicitly. This would catch leakage at the source.

### 23. Literal Types

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

### 24. Formal Grammar

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

`@` is `ImmediateAt` — emitted when it appears directly after any non-whitespace token (identifier, `]`, string literal, number, etc.) with no preceding whitespace. This distinguishes `x@Integer` (annotation) from `x @ Int` (which would be parsed differently if `@` were a regular operator). Any value-producing token can be immediately annotated: `[f x]@Type`, `"str"@Type`, `42@Type`, `obj.field@Type` are all valid.

### 25. Mixed-Stage Routing

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
| `x@Integer` | Type | `Type::Int` (via `expand_named` → `typenode_value_to_type`) |
| `x@[or Int Null]` | Type-stage eval | `Type::Union(...)` (via `eval_type_stage_expr` → `typenode_value_to_type`) |
| `x@[type: Int  default: 0]` | Split | `type:` → type-stage, `default:` → runtime |
| `fn@[bind: [a]  return: a  constraint: ...]` | Split | Per step table above |
| `[@Integer expr]` | Type + runtime | Type assertion at materialization |
| `x@[is: pred]` in match | Runtime | Soft guard at match time |
| `x@[repr: "u8"]` | Runtime | Materialization boundary check |

### 26. Type Inference and Let-Generalization

Tinct uses Hindley-Milner inference with row polymorphism and levels-based let-generalization (Kiselyov 2013).

**TypeVar levels.** Each TypeVar carries an integer level representing the nesting depth of its binding scope. In the `Type` enum, this is `Type::TypeVar(name: String, level: u32)`; in the tinct-stage value representation it is `TypeNode.TypeVar { name: String  level: Int }`. `state.levels` maps TypeVar name to its authoritative current level (updated by level lowering); the level carried in the variant is the creation-time level only. Let-generalization uses `state.levels[name] > enclosing_level` — always use `state.levels`, never the creation-time level. TypeVars whose level exceeds the current enclosing level are generalized into the polymorphic scheme — preventing TypeVars from escaping their scope.

**Dict letrec inference.** Dict entries form a letrec scope. The type checker runs five passes:

1. **Pass 0:** Resolve key names (literal keys extracted; computed keys resolved via type inference in parent scope)
2. **Pass 1:** Bind all non-alias key names to fresh TypeVar placeholders. For `[fn ...]` entries, the placeholder is `Type::Function { params: [(name, fresh TypeVar)...], ret: fresh TypeVar, variadic, required_count }` — enabling recursive calls to resolve the function shape without a return annotation. For non-fn entries, the placeholder is a bare fresh TypeVar as before.
3. **Pass 2:** Register type aliases sequentially (each alias sees previously registered siblings)
4. **Pass 3:** Infer actual value types and bind Pass 1 placeholders in the substitution. For fn entries, the pre-bound ret TypeVar is directly inserted into `subst.type_map` (not unified — `infer_dict` is sync; `unify` is async), resolving recursive references to the concrete return type.
5. **Pass 4:** Generalize field types into polymorphic schemes

**Substitution idempotence invariant.** `Substitution::apply()` is idempotent — applying the same substitution twice yields the same result. Achieved by transitive chaining in `apply_inner()` (Robinson 1965).

**Alpha-equivalence.** Variable names are significant at the source level but irrelevant for principal types. `instantiate()` performs alpha-renaming (generating fresh `_t0`, `_t1`, ...) at each call site to prevent unintended unification between independent uses of the same polymorphic function.

**Constraint propagation.** When `bind(TypeVar(α), TypeVar(β))` occurs during unification, class constraints on `α` are transferred to `β` (deduplicated). This preserves the principal type property: the representative TypeVar accumulates all constraints from its equivalence class. `HasField` constraints are NOT transferred — they encode position-specific field structure.

See [Type Inference](06-type-inference.md) for the full let-generalization algorithm and constraint solving details.

### 27. Reflection and `ast-of`

`ast-of` on an annotated expression returns the resolved type as a `TypeNode` value alongside the expression's AST structure. The `TypeNode` value is produced by converting the annotation resolver's `Type` result back through `typenode_value_to_type` in reverse — the tinct-facing representation of the type the checker resolved.

```tinct
ast-of: [fn@a  min: [fn@[bind: [a]  return: a  constraint: [a: Comparable]] [xs@[Seq a]] ...]]
# → TypeNode.Arrow {
#     params: [TypeNode.TypeApplication { ctor: TypeNode.TypeConstructor "Seq"
#                                         args: [TypeNode.TypeVar { name: "_t0" level: 1 }] }]
#     result: TypeNode.TypeVar { name: "_t0" level: 1 }
#   }
#   constraints: [{ class: "Comparable" var: "_t0" }]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 39:1:7
  |
  1 | ast-of: [fn@a  min: [fn@[bind: [a]  return: a  constraint: [a: Comparable]] [xs@[Seq a]] ...]]
    |       ^
```

`ast-of` on a default: value returns the unevaluated AST expression dict — not the evaluated default value. This enables tooling to inspect the source expression of defaults without forcing evaluation.

---

## Part VIII: Refinement Types and Type-Level Lookup Tables

### 27. `[Bytes N]` — Fixed-Size Byte Sequences

`[Bytes N]` is a refinement type: `Bytes` refined by the constraint `length = N`. `[Bytes N] <: Bytes` — a fixed-size sequence is a valid `Bytes` value usable anywhere variable-length `Bytes` are accepted.

```tinct
key@[Bytes 32]:  my-bytes       # TypeAssert validates length = 32 at runtime
addr@[Bytes 4]:  ip-bytes       # [Bytes 4] — 4-byte IPv4 address
nonce:           [crypto-random 12]  # inferred [Bytes 12]
```

**Type-stage arithmetic** propagates through size expressions:

```tinct
# [concat a b] where a: [Bytes 32] and b: [Bytes 32] → [Bytes 64]
# Type-stage: [+ 32 32] → 64 at type-check time
```

`N` is always a concrete type-stage integer — always a literal or arithmetic expression over literals evaluated at type-check time. When the size is genuinely unknown, use `Bytes` (no size constraint). There are no type-stage size variables.

**Implementation:** `[Bytes N]` is `TypeNode.SizedBytes { n: Int }` with a `supertype: TypeNode.Bytes` annotation. Subtyping `[Bytes N] <: Bytes` is handled generically via the `supertype:` field — no Rust special-case. Size inequality (`M ≠ N`) fails via structural equality on the `n` field. Runtime representation is `Value::Bytes`; size validation occurs at `TypeAssert` boundaries via the `is:` predicate.

### 28. Type-Level Lookup Tables

Variant declarations carry named compile-time constants (using `:` syntax) alongside or instead of runtime payload fields (using `@` syntax). Constants are resolved at type-check time and stored as metadata on the constructor:

**Grammar:**
```
variant_constructor = "[" UppercaseIdent (constant_entry | payload_field)* "]"
constant_entry      = lowercase_ident ":" literal_expr    # compile-time value
payload_field       = lowercase_ident "@" type_expr       # runtime payload
```

**Example:**

```tinct
DnsRcode: [type
  [NoError  rcode: 0  description: "No Error"]
  [FormErr  rcode: 1  description: "Format Error"]
  [ServFail rcode: 2  description: "Server Failure"]]

WsFrame: [type
  [Text   opcode: 0x01  data@String]   # constant + payload field
  [Binary opcode: 0x02  data@Bytes]
  [Close  opcode: 0x08  code@WsCloseCode  reason@String]]
```

**Forward lookup:** dot-access on a variant value or type name:

```tinct
DnsRcode.ServFail.rcode         # → 2
some-rcode.rcode                # → the rcode constant for the runtime variant
frame.opcode                    # → 0x01 for Text, etc. — no match needed
```

**Reverse lookup** via generalized `Indexable`:

```tinct
[get rcode: 2 DnsRcode]         # → DnsRcode.ServFail
[get rcode: 99 DnsRcode]        # → Absent.Absent
```

`DnsRcode` in lookup position evaluates to a runtime `Seq` of all variants, each carrying its compile-time constants as accessible fields. `get` finds the first variant where all selector fields match.

This eliminates all `*->int`/`int->*` lookup functions — the constants travel with the type and cannot get out of sync.

---

## Reference

- **Kiselyov, O. (2013).** "Efficient and Insightful Generalization." — [levels-based let-generalization; TypeVar level assignment; `state.levels` is authoritative current level, payload carries creation-time level]
- **Robinson, J.A. (1965).** "A Machine-Oriented Logic Based on the Resolution Principle." _JACM_, 12(1), 23–41. — [unification; substitution idempotence]
- **Gaster, B.R. & Jones, M.P. (1996).** "A Polymorphic Type System for Extensible Records and Variants." — [named parameters in calling convention; names not part of principal type]
- **Amadio, R.M. & Cardelli, L. (1993).** "Subtyping Recursive Types." _ACM TOPLAS_, 15(4), 575–631. — [foundational coinductive subtype algorithm; S-Assum/S-Hyp rules; equirecursive type equality]
- **Chau, T. & Parreaux, L. (2026).** "Boolean-Algebraic Subtyping with Equirecursive Types." §3.3.1. — [S-Exp + S-Assum framework proven sound for BAS union/intersection/negation; sigma threading through all arms]
- **Pierce, B.C. (2002).** _Types and Programming Languages._ MIT Press. §21 "Recursive Types." — [equirecursive vs isorecursive; rational tree representation; simultaneous-opening for recursive type unification]
- **Huet, G. (1976).** _Résolution d'Équations dans des Langages d'Ordre 1, 2, ..., ω._ Ph.D. thesis, Université Paris VII. — [rational tree unification; finite representation of infinite cyclic types]
- **Jones, M.P. (1995).** _Qualified Types: Theory and Practice._ Cambridge. — [constraint schemes; processing order]
- **Jones, M.P. (2000).** "Type Classes with Functional Dependencies." _ESOP 2000_. — [fundep improvement; MPTC constraint propagation]

See [References](17-references.md) for complete citations.
