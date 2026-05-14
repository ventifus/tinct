# Type Annotations & Type Expressions

## Overview

**Mandatory, bottom-up type inference with annotation-driven polymorphism, inspired by Hindley-Milner.** Every value has a type. Row polymorphism for dicts. Type errors raised early — good for LLMs and LSP feedback. Let-generalization uses levels-based approach (Kiselyov 2013) for polymorphic let-bindings — see [Type Inference](06-type-inference.md) §Let-Generalization. Polymorphism arises from type variable annotations (e.g., `x@a`); let-generalization makes these polymorphic across binding sites. See the [References](17-references.md) for details.

**Annotations are optional but enforced.** The compiler infers types when annotations are omitted. If you write an annotation, it's a contract — the compiler checks the actual type matches and raises an error on mismatch.

## `@` Property Annotations

**`@` attaches a property dict** to a parameter or function. Shorthand: bare word after `@` means `[type: BareWord]`.

**`@` is always a structural separator.** It is not a valid bare word character. Wherever `@` appears immediately after a bare word (no whitespace), it separates the word from an annotation value. Strings containing `@` must be quoted: `"email@example.com"`.

```tinct
# Shorthand — type only
x@Number                              # equivalent to x@[type: Number]

# Full form — type + other properties
timeout@[type: Number  default: 30]   # named param with default

# On fn — return type
[fn@Number [x@Number] ...]            # shorthand: returns Number
[fn@[return: Number  doc: "Sum"] ...]   # full form
```

**Parameter properties:**

| Property | Meaning |
|----------|---------|
| `type` | Compile-time type (the common case, covered by shorthand) |
| `default` | Default value — makes the parameter named/optional |
| `doc` | Human-readable description — surfaced in LSP hover, ignored by the type checker |
| `is` | Runtime predicate — `Fn@Bool [Any]`; value must return `true` for the annotation to pass. Used in match arm guards and structural contracts. |
| `repr` | Numeric representation constraint — enforces integer bit width and signedness. Accepts `"u8"`, `"i8"`, `"u16"`, `"i16"`, `"u32"`, `"i32"`, `"u64"`, `"i64"`. Type checker verifies the annotated expression has a numeric type (Int, Float, or Number). |

**Arbitrary keys are allowed.** The core system reads `type:`, `default:`, and `repr:` and ignores everything else. Programmers may add any metadata keys they find useful — `doc:`, `is:`, `example:`, `deprecated:`, etc. Tooling can read these at the AST or annotation level. Unknown keys are never an error.

**Any parameter is nameable at the call site** (Kotlin model). A parameter with `default:` is optional — it uses the default value when neither a positional nor named argument covers it. A parameter without `default:` is required — it must be covered by either a positional argument at its index or a named argument. Required and optional parameters may be freely interleaved in the parameter list.

```tinct
fetch: [fn@String [url@String
                   timeout@[type: Number  default: 30]
                   retries@[type: Number  default: 3]]
    ...]

# Call with bare key-value named args
[fetch "https://example.com" timeout: 60]
# url = "https://example.com", timeout = 60, retries = 3 (default)
```

**Named args at the call site** are bare `key: value` pairs inside the call brackets. This is natural — the call expression is a dict, with integer-keyed entries for positional args and string-keyed entries for named args.

### Formal Grammar

**In parameter position** (inside a `param_list`):
```ebnf
param_annotation = ${ "@" ~ annotation_value }
```
`x@Number` splits into param `x` with annotation `Number`.

**On `fn` keyword** (return type):
```ebnf
fn_annotation = ${ "@" ~ annotation_value }
```
`fn@Number` means the function returns `Number`.

**In value position** (generalized annotation):
```ebnf
annotated_bare = ${ bare_word ~ "@" ~ annotation_value }
```
`Fn@Number` produces an `Annotated` node with name `"Fn"` and annotation `Number`. This is used for function type constructors (`Fn@Return [Params]`) and is available for future use on any bare word.

## `@` on Expressions — Type Assertions

**`[@Type expr]` is a type assertion expression.** Materializes the value, checks its type, throws on mismatch. No `as` keyword needed — `@` handles it.

```tinct
data: [from-json input]        # type: Any

# Type assertion — throws if wrong
name: [@String data.name]

# Inline in a call
[+ [@Number x] 1]

# Complex type
users: [@[Person] [from-json input]]
```

**With property dict — safe cast with fallback:**

```tinct
# Returns "anonymous" if type check fails (no exception)
name: [@[type: String  default: "anonymous"] data.name]

# Returns 8080 if not a valid number
port: [@[type: Number  default: 8080] config.port]
```

**`default:` meaning by context:**

| Context | `default:` meaning |
|---------|-------------------|
| Function parameter | Value used when caller omits the argument |
| `@` expression | Fallback if type assertion fails |

Both are "fallback value when the expected thing isn't there."

**Parsing rule:** Inside `[...]`, if the first token is `@`, it's a type assertion expression. Unambiguous — `@` is not a valid start for a bare word or variable reference, so it can't appear as the first element of a `[]` in any other context.

### Formal Grammar

**As type assertion** (first token inside `[]`):
```ebnf
type_assert_body = { "@" ~ annotation_value ~ value }
```
`[@Number expr]` asserts `expr` has type `Number`. When a `default:` is provided (e.g., `[@[type: Number  default: 0] expr]`), the default value is evaluated in the same environment as the asserted expression.

## Return Type on `fn@`

**`fn@Type` declares the return type.** Optional — inferred if omitted. Enforced if specified.

```tinct
# Return type annotated — compiler checks body matches
double: [fn@Number [x@Number] [* x 2]]

# Return type omitted — compiler infers Number
double: [fn [x@Number] [* x 2]]

# Wrong return type — compile error
double: [fn@String [x@Number] [* x 2]]    # Error: body returns Number, not String
```

**`Fn@Return [Params]` for function types.** Function type expressions mirror function definitions:

```tinct
# Definition:  fn@Return [params] body
[fn@Number [x@Number y@Number] [+ x y]]

# Type:        Fn@Return [ParamTypes]
[Fn@Number [Number Number]]
```

`Fn` is uppercase (concrete type constructor convention). The return type attaches via `@`, matching `fn@Type`. Parameter types go in brackets, matching the param list in definitions. All types must be specified — there is no body to infer from.

```tinct
[Fn@b [a]]              # function from a to b
[Fn@Bool [a]]           # predicate: a to Bool
[Fn@c [a b]]            # two-arg function: a, b to c
[Fn@[Fn@c [b]] [a]]    # higher-order: a to (b to c)
```

**`...` for open records** (row polymorphism):

```tinct
# Open — at least these keys, possibly more
[name: String ...]

# Closed — exactly these keys
[name: String  age: Number]

# Named row variable (advanced)
[name: String ...rest]
```

**Type aliases via `[type ...]`** — textual expansion with free variables connecting by name:

```tinct
[
  Mapper: [type [Fn@b [a]]]
  Predicate: [type [Fn@Bool [a]]]
  Person: [type [name: String  age: Number]]

  map: [fn@[b] [f@Mapper  xs@[a]] ...]
  filter: [fn@[a] [pred@[Fn@Bool [a]]  xs@[a]] ...]
  greet: [fn@String [p@Person] ...]
]
```

**Type conventions:**
- Uppercase: concrete types (`String`, `Number`, `Person`)
- Lowercase: type variables (`a`, `b`, `k`, `v`)
- `String` / `Str`: `String` is the user-facing type name used in annotations; `Str` is the internal `Type::Str` variant name in the implementation. Both refer to the same type. Use `String` in annotations and prose; `Str` appears in type inference output and error messages.
- `Any`: escape hatch for dynamic data
- `Null`: empty record `[]` — represents void/unit return type. `@Null` resolves to `Type::Record` with no fields (the closed empty-record type). Use `fn@Null` for functions that return no meaningful value.
- `Seq`: lazy sequence type. `[@Seq expr]` in TypeAssert position checks that `expr` is a Seq (element type is `Any`); the `@ElemType` suffix (e.g. `[@Seq@String expr]`) is a **parse error** in TypeAssert brackets — only bare `@Seq` is supported there. To constrain the element type, use the standalone expression form `Seq@String` (an `Annotated` bare word; note: `xs@Seq@String` in parameter annotation position is also a parse error — the parser captures only one `@TypeName` per parameter), which resolves to `Type::Seq(Type::Str)`.
- `[@Type expr]`: type assertion / runtime cast from `Any`
- `[Fn@Return [ParamTypes]]`: function type (mirrors `fn@Return [params]`)
- `@Dict`: open record type. Each `@Dict` annotation allocates a fresh open record (`Type::Record(Row { fields: {}, tail: RowVar(_t{n}) })`) per annotation site. Independent constraints: `x@Dict y@Dict` creates two distinct row variables, allowing each parameter to accept different record structures without unification coupling.

**Literal types.** Integer and string literals carry their value in the type: `42` has type `IntLiteral(42)`, `"hello"` has type `StringLiteral("hello")`. Literal types are subtypes of their base types: `IntLiteral(n)` <: `Int` <: `Number`, `StringLiteral(s)` <: `String`. All bindings in Tinct are immutable, so literal types never widen implicitly -- they widen only when an annotation demands the base type. Float and Bool literals do not need literal type variants: float equality is fragile (rounding) and NaN is not reflexively equal, so computed key resolution on float literals would be misleading; Bool only has two values and is trivially enumerable without a literal type.

**Literal types enable computed key resolution.** When a dict has a computed key like `[$k: 42]`, the type checker infers the type of `k` in the parent scope. If it resolves to a literal type (e.g., `StringLiteral("name")`), the type checker extracts the literal value and uses it as the field name. If the key expression resolves to a non-literal type (e.g., `String`) or `Any`, the type checker cannot determine the field name statically -- the entry's value is still type-checked, but the field is excluded from the Record type. This is the conservative correct behavior: the Record only contains fields whose names are statically known.

```tinct
[k: "hello"  $k: 42]       # type: [k: StringLiteral("hello")  hello: IntLiteral(42)]
                            # k resolves to StringLiteral("hello") → field name "hello"

[k: "hello"]
[$k: 42]                    # scope chain: k resolves from parent → field "hello"

[k: dynamic  $k: 42]        # k has type String (not literal) → field excluded from Record
```

**Dict values are never type-annotated.** Always inferred from literals/expressions.

**Type inference for letrec dicts:** Dict entries form a letrec scope where all keys are visible to all values. The type checker handles this in five passes (Pass 0–4): (0) resolve key names — literal keys extracted directly, computed keys resolved via type inference in parent scope, (1) bind all resolved non-alias key names to fresh type variables at the current level (`state.level`, not `Any`) to enable unification constraints during forward references, (2) register type aliases sequentially (each sees previously registered siblings), (3) infer actual value types and unify them with their bound type variables from Pass 1, (4) generalize field types into polymorphic schemes where applicable. Pass 1 fresh type variable binding (not `Any` binding) is load-bearing for let-generalization: forward references participate in unification constraints, allowing type information to flow bidirectionally. Polymorphic function calls use Hindley-Milner unification: each call site instantiates fresh type variables, unification binds them against argument types, and the substitution is applied to determine the return type. Computed keys whose type is not a literal are excluded from the Record's fields but their values are still type-checked.

**Substitution idempotence invariant.** `Substitution::apply()` is idempotent: applying the same substitution twice yields the same result as applying it once. This is achieved by transitive chasing in `apply_inner()` — when resolving a type variable, the substitution follows the binding chain to a fixpoint rather than performing a single lookup. This guarantees that `apply(apply(ty, s), s) = apply(ty, s)` for all types `ty` and substitutions `s`, which is a standard requirement for unification-based type inference (Robinson, 1965).

**Alpha-equivalence and variable naming.** Variable names are significant in tinct — `[fn [x] x]` and `[fn [y] y]` are not alpha-equivalent at the source level. The type checker uses string-based variable lookup, so type variables introduced by annotations bind by name. However, `instantiate()` performs alpha-renaming by generating fresh names (`_t0`, `_t1`, ...) for each call site, ensuring that polymorphic function types do not share type variables across independent call sites. This is a deliberate choice: source-level names matter for readability and error messages, while inference-time freshening prevents unintended unification between call sites.

**Type alias entries are excluded from record fields.** A `[type ...]` entry registers an alias in the type environment but does not contribute a field to the enclosing record's type. This matches the evaluator, which returns an empty dict for type alias entries.

**Function type param lists:** `[Fn@Return [ParamTypes]]` is the full function type syntax. The type checker resolves both the return type annotation and parameter type list, producing `Type::Function { params, ret }`. All types in the param list must be specified explicitly.

## fn@[...] Function Metadata Dict

`fn@[...]` accepts a named-key metadata dict with three optional keys:

| Key | Value | Semantics |
|-----|-------|-----------|
| `return:` | any type annotation | The function's return type |
| `constraint:` | `[typevar: ClassName ...]` | TypeVar constraints enforced at call sites |
| `doc:` | string literal | Documentation string surfaced in LSP hover |

All three keys are optional. `fn@[...]` with no `return:` key infers the return type from the body. The existing `fn@Type` shorthand is permanent and equivalent to `fn@[return: Type]`.

**Disambiguation.** If any entry has a recognized metadata key (`return:`, `constraint:`, or `doc:`), the dict is treated as a metadata dict. If ALL entries are positional, the dict is a union return type (`fn@[Int Null]` → function returning `Int | Null`). Mixed recognized metadata keys with positional entries is a type error: "fn annotation must use either named keys or positional entries, not both." An unrecognized named key alongside a recognized key is a type error: "unknown function annotation key". A dict with only unrecognized named keys (e.g. `fn@[name: Str]`) is interpreted as a record return type annotation.

```tinct
# Shorthand — unchanged, always valid
min: [fn@a [xs@Seq@a] ...]

# Full form — return type, constraint, and doc string
min: [fn@[return: a  constraint: [a: Comparable]  doc: "Return smallest element"] [xs@Seq@a] ...]

# Doc-only — return type inferred from body
greet: [fn@[doc: "Format a greeting"] [name@String] [str "Hello " name]]

# Constraint on TypeVar not used as return type
check-all: [fn@[return: Bool  constraint: [a: Equatable]] [xs@Seq@a  target@a] ...]

# Positional union (all-positional form — not a metadata dict)
find: [fn@[Int Null] [xs@Seq@Int  target@Int] ...]
```

**Constraint syntax.** The `constraint:` value is a dict using the binding form `[typevar: ClassName]`. Lowercase keys name TypeVars; uppercase values name constraint classes.

```tinct
# Single constraint
constraint: [a: Comparable]

# Multiple TypeVars with different constraints
constraint: [a: Comparable  b: Showable]

# Multiple constraints on one TypeVar (list value)
constraint: [a: [Comparable Showable]]
```

**Processing order.** `fn@[...]` resolves keys in a fixed internal order: `constraint:` first (registers TypeVars in `ann_mapping` with constraint registration in `state.constraints`), then `return:` (which may reference those TypeVars), then `doc:`. Source key order does not matter — the resolver uses this canonical sequence.

**TypeVar scoping.** Constraint TypeVars share the `ann_mapping` mechanism with parameter annotations. A TypeVar named in `constraint:` is the same TypeVar referenced by the same name in `return:` or in a parameter annotation:

```tinct
# `a` appears in constraint:, return:, and xs@Seq@a — all the same TypeVar
min: [fn@[return: a  constraint: [a: Comparable]] [xs@Seq@a] ...]
```

**Interaction with inference.** Explicit constraints compose with inferred constraints. If `constraint: [a: Comparable]` is declared and the body also uses `a` in an `Equatable` context, both are registered; constraint simplification removes `Equatable a` since `Comparable` entails it via the superclass relation. If the declared constraint is stronger than what the body uses, it is still enforced at call sites — allowing library authors to declare a stricter interface than the body exercises.

**`doc:` and LSP hover.** The doc string is stored in `TypeScheme.doc` and displayed in LSP hover below the inferred type signature. It does not affect type checking or runtime behavior.

```
min: Comparable a => Fn@a [Seq@a]
Return smallest element
```

**Examples:**

```tinct
# Constraint on the return TypeVar
min: [fn@[return: a  constraint: [a: Comparable]] [xs@Seq@a] ...]
# Inferred: Comparable a => Fn@a [Seq@a]

# Multiple TypeVars with different constraints
compare: [fn@[return: Bool  constraint: [a: Comparable  b: Showable]] 
          [x@a  y@a  logger@b] ...]
# Inferred: (Comparable a, Showable b) => Fn@Bool [a a b]

# Multiple constraints on one TypeVar
display-sorted: [fn@[return: String  constraint: [a: [Comparable Showable]]]
                 [xs@Seq@a] ...]
# Inferred: (Comparable a, Showable a) => Fn@String [Seq@a]

# Constraint on TypeVar not used as return type
check-all: [fn@[return: Bool  constraint: [a: Equatable]]
            [xs@Seq@a  target@a] ...]
# Inferred: Equatable a => Fn@Bool [Seq@a a]

# Doc-only annotation — return type inferred from body
greet: [fn@[doc: "Format a greeting string"] [name@String] [str "Hello " name]]
# Hover: Fn@String [String]
#        Format a greeting string

# Constraint + doc + return
between: [fn@[return: Fn@Bool [a]
              constraint: [a: Comparable]
              doc: "Return a predicate testing whether a value lies in [lo, hi)"]
          [lo@a  hi@a]
          [fn@Bool [x@a] [and [>= x lo] [< x hi]]]]
# Hover: Comparable a => Fn@Fn@Bool [a] [a a]
#        Return a predicate testing whether a value lies in [lo, hi)
```

**Label TypeVar annotations.** Two forms introduce a label TypeVar of kind `Label`:

- `key@Label` — anonymous label TypeVar; the type checker generates a fresh name internally. Use when the label name is not referenced elsewhere in the type.
- `key@[label: l]` — named label TypeVar `l`; use when the same label must appear in multiple type positions (e.g., two parameters that must access the same field).

```tinct
# Anonymous form — get/get-or use this
[fn@[return: a] [key@Label  dict@d] [get key dict]]
# inferred: ∀ (l : Label) d a. HasField l d a => Label(l) → d → a

# Named form — when the same label appears twice
[fn@[return: a] [key@[label: l]  default@a  dict@d] ...]
```

`HasField` constraints are never written explicitly — the type checker generates them from the label annotation on `key`. See [Type Inference](06-type-inference.md) §Higher-Kinded Types and Type Classes §HasField.

### Type Expressions

Type expressions appear in type annotations and `[type ...]` declarations. They use the same `[]` syntax as data but are distinguished by context (after `@`, inside `type` form).

**Function types** use `Fn@Return [ParamTypes]`, mirroring function definitions (`fn@Return [params] body`):
```tinct
[Fn@b [a]]              # function from a to b
[Fn@Bool [a]]           # predicate
[Fn@c [a b]]            # two-arg function
```

The parser handles this via the `annotated_bare` rule -- `Fn@b` parses as `Annotated { name: "Fn", annotation: Simple("b") }`. The type checker interprets `Fn` as a function type constructor. All types in a type definition must be explicit -- there is no body to infer from.

**Note:** `Fn@Number` in a bare context (not inside `[]`) is also valid and parsed via the `annotated_bare` grammar rule, producing the same AST structure.

**Row polymorphism and width subtyping.** Under BAS (Boolean-Algebraic Subtyping), all records are closed — there are no row variables. Openness is expressed via width subtyping: a record with more fields is a subtype of one with fewer fields, so a function annotated `@[name: String]` accepts any record that has at least a `name: String` field. The `...` and `...name` rest entry forms are valid syntax and express user intent for openness, but they produce the same closed record type — width subtyping handles the structural openness automatically without row variables.

**Type conventions** (not enforced by parser, enforced by type checker):
- Uppercase first letter = concrete type (`String`, `Number`, `Person`, `Fn`)
- Lowercase first letter = type variable (`a`, `b`, `k`, `v`)
- `Any` = dynamic escape hatch

**Type inference context.** The type system uses type schemes (`∀α₁...αₙ. τ`) for polymorphic bindings via levels-based let-generalization (Kiselyov 2013). Type variables carry an integer level for scope tracking (`TypeVar(String, u32)`). These are type checker internals — the parser produces bare type names as strings. See [Type Inference](06-type-inference.md) §Let-Generalization for details.

### Recursive Type Aliases

**Equi-recursive type aliases are supported** via two-pass registration. A type alias may reference itself in its body, enabling recursive data structures:

```tinct
# Linked list
List: [type [head: Int  tail: List]]

# Binary tree
Tree: [type [value: Int  left: Tree  right: Tree]]

# Mutually recursive types (must be in the same dict)
A: [type [b_field: B]]  B: [type [a_field: A]]
```

**Two-pass registration.** All type aliases in a dict are pre-registered with placeholder bodies before resolving actual bodies. This enables forward references and self-references:

1. **Pass 1 (pre-registration):** All `[type ...]` entries are registered with `Type::Unknown` placeholder bodies.
2. **Pass 2 (resolution):** Each alias body is resolved with the alias itself visible in the environment.

**Cycle detection.** When resolving an alias body, if the alias references itself (directly or indirectly), the recursive reference resolves to `Type::Unknown`. This breaks the cycle while allowing the structure to be defined. A recursion guard (`HashSet<String>`) tracks aliases currently being expanded.

**Depth limit.** Alias expansion is limited to 256 layers (MAX_ALIAS_DEPTH). Exceeding this limit produces the error: `recursive type alias 'Name' exceeds maximum unfolding depth (256)`.

**Semantics: equi-recursive, not iso-recursive.** Type aliases are transparent — they unfold automatically during type checking. There is no explicit `fold`/`unfold` syntax (iso-recursive semantics). This matches Amadio & Cardelli (1993) equi-recursive type equality with a depth guard for decidability.

**Recursive types:** Parameterized type aliases support recursive algebraic data types. A type alias can reference itself in its body, with the recursive reference resolved through the alias table. The recursion guard prevents infinite expansion, and the depth limit (256 layers) provides decidability. For configuration use cases, recursive types are uncommon — this feature primarily supports self-hosting stdlib functions that operate on tree-like structures.

**Example:**

```tinct
# Define a recursive list type
List: [type [head: Int  tail: List]]

# Use in annotation
mylist@List: [head: 1  tail: [head: 2  tail: []]]

# The tail field has type Unknown (recursive placeholder)
# but the overall structure is recognized as a List
```
