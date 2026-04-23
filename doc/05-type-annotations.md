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
[fn@[type: Number  doc: "Sum"] ...]   # full form
```

**Parameter properties:**

| Property | Meaning |
|----------|---------|
| `type` | Compile-time type (the common case, covered by shorthand) |
| `default` | Default value — makes the parameter named/optional |

Future properties (extensible without syntax changes): `validate`, `doc`.

**Any parameter is nameable at the call site** (Kotlin model). A parameter with `default:` is optional — it uses the default value when neither a positional nor named argument covers it. A parameter without `default:` is required — it must be covered by either a positional argument at its index or a named argument. Required and optional parameters may be freely interleaved in the parameter list.

```tinct
fetch: [fn@String [url@String
                   timeout@[type: Number  default: 30]
                   retries@[type: Number  default: 3]]
    ...]

# Call with bare key-value named args
[call $fetch "https://example.com" timeout: 60]
# $url = "https://example.com", $timeout = 60, $retries = 3 (default)
```

**Named args at the call site** are bare `key: value` pairs inside `[call ...]`. This is natural — the call expression is a dict, with integer-keyed entries for positional args and string-keyed entries for named args.

### Formal Grammar

**In parameter position** (inside a `param_list`):
```pest
param_annotation = ${ "@" ~ annotation_value }
```
`x@Number` splits into param `x` with annotation `Number`.

**On `fn` keyword** (return type):
```pest
fn_annotation = ${ "@" ~ annotation_value }
```
`fn@Number` means the function returns `Number`.

**In value position** (generalized annotation):
```pest
annotated_bare = ${ bare_word ~ "@" ~ annotation_value }
```
`Fn@Number` produces an `Annotated` node with name `"Fn"` and annotation `Number`. This is used for function type constructors (`Fn@Return [Params]`) and is available for future use on any bare word.

## `@` on Expressions — Type Assertions

**`[@Type $expr]` is a type assertion expression.** Materializes the value, checks its type, throws on mismatch. No `as` keyword needed — `@` handles it.

```tinct
data: [call $from-json $input]        # type: Any

# Type assertion — throws if wrong
name: [@String $data.name]

# Inline in a call
[call $+ [@Number $x] 1]

# Complex type
users: [@[Person] [call $from-json $input]]
```

**With property dict — safe cast with fallback:**

```tinct
# Returns "anonymous" if type check fails (no exception)
name: [@[type: String  default: anonymous] $data.name]

# Returns 8080 if not a valid number
port: [@[type: Number  default: 8080] $config.port]
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
```pest
type_assert_body = { "@" ~ annotation_value ~ value }
```
`[@Number $expr]` asserts `$expr` has type `Number`. When a `default:` is provided (e.g., `[@[type: Number  default: 0] $expr]`), the default value is evaluated in the same environment as the asserted expression.

## Return Type on `fn@`

**`fn@Type` declares the return type.** Optional — inferred if omitted. Enforced if specified.

```tinct
# Return type annotated — compiler checks body matches
double: [fn@Number [x@Number] [call $* $x 2]]

# Return type omitted — compiler infers Number
double: [fn [x@Number] [call $* $x 2]]

# Wrong return type — compile error
double: [fn@String [x@Number] [call $* $x 2]]    # Error: body returns Number, not String
```

**`Fn@Return [Params]` for function types.** Function type expressions mirror function definitions:

```tinct
# Definition:  fn@Return [params] body
[fn@Number [x@Number y@Number] [call $+ $x $y]]

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
- `Any`: escape hatch for dynamic data
- `[@Type $expr]`: type assertion / runtime cast from `Any`
- `[Fn@Return [ParamTypes]]`: function type (mirrors `fn@Return [params]`)

**Literal types.** Integer and string literals carry their value in the type: `42` has type `IntLiteral(42)`, `"hello"` has type `StringLiteral("hello")`. Literal types are subtypes of their base types: `IntLiteral(n)` <: `Int` <: `Number`, `StringLiteral(s)` <: `String`. All bindings in Tinct are immutable, so literal types never widen implicitly -- they widen only when an annotation demands the base type. Float and Bool literals do not need literal type variants because they cannot be used as dict keys.

**Literal types enable computed key resolution.** When a dict has a computed key like `[$k: 42]`, the type checker infers the type of `$k` in the parent scope. If it resolves to a literal type (e.g., `StringLiteral("name")`), the type checker extracts the literal value and uses it as the field name. If the key expression resolves to a non-literal type (e.g., `String`) or `Any`, the type checker cannot determine the field name statically -- the entry's value is still type-checked, but the field is excluded from the Record type. This is the conservative correct behavior: the Record only contains fields whose names are statically known.

```tinct
[k: hello  $k: 42]       # type: [k: StringLiteral("hello")  hello: IntLiteral(42)]
                          # $k resolves to StringLiteral("hello") → field name "hello"

[k: hello]
[$k: 42]                  # scope chain: $k resolves from parent → field "hello"

[k: $dynamic  $k: 42]    # $k has type String (not literal) → field excluded from Record
```

**Dict values are never type-annotated.** Always inferred from literals/expressions.

**Type inference for letrec dicts:** Dict entries form a letrec scope where all keys are visible to all values. The type checker handles this in four passes: (0) resolve key names -- literal keys extracted directly, computed keys resolved via type inference in parent scope, (1) bind all resolved key names to `Any`, (2) register type aliases sequentially (each sees previously registered siblings), (3) infer actual value types. Forward references resolve to `Any`. Polymorphic function calls use Hindley-Milner unification: each call site instantiates fresh type variables, unification binds them against argument types, and the substitution is applied to determine the return type. Computed keys whose type is not a literal are excluded from the Record's fields but their values are still type-checked.

**Substitution idempotence invariant.** `Substitution::apply()` is idempotent: applying the same substitution twice yields the same result as applying it once. This is achieved by transitive chasing in `apply_inner()` — when resolving a type variable, the substitution follows the binding chain to a fixpoint rather than performing a single lookup. This guarantees that `apply(apply(ty, s), s) = apply(ty, s)` for all types `ty` and substitutions `s`, which is a standard requirement for unification-based type inference (Robinson, 1965).

**Alpha-equivalence and variable naming.** Variable names are significant in tinct — `[fn [x] $x]` and `[fn [y] $y]` are not alpha-equivalent at the source level. The type checker uses string-based variable lookup, so type variables introduced by annotations bind by name. However, `instantiate()` performs alpha-renaming by generating fresh names (`_t0`, `_t1`, ...) for each call site, ensuring that polymorphic function types do not share type variables across independent call sites. This is a deliberate choice: source-level names matter for readability and error messages, while inference-time freshening prevents unintended unification between call sites.

**Type alias entries are excluded from record fields.** A `[type ...]` entry registers an alias in the type environment but does not contribute a field to the enclosing record's type. This matches the evaluator, which returns an empty dict for type alias entries.

**Function type param lists:** `[Fn@Return [ParamTypes]]` is the full function type syntax. The type checker resolves both the return type annotation and parameter type list, producing `Type::Function { params, ret }`. All types in the param list must be specified explicitly.

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

**Row polymorphism** is supported via `rest_entry` syntax in type expressions. `...` marks an open record type (any additional fields are permitted), and `...name` introduces a named row variable for polymorphic record operations:

```tinct
[name: String ...]            # open record: has name, allows other fields
[name: String ...r]           # named row variable r captures the remaining fields
```

**Type conventions** (not enforced by parser, enforced by type checker):
- Uppercase first letter = concrete type (`String`, `Number`, `Person`, `Fn`)
- Lowercase first letter = type variable (`a`, `b`, `k`, `v`)
- `Any` = dynamic escape hatch

**Type inference context.** The type system uses type schemes (`∀α₁...αₙ. τ`) for polymorphic bindings via levels-based let-generalization (Kiselyov 2013). Type variables carry an integer level for scope tracking (`TypeVar(String, u32)`). These are type checker internals — the parser produces bare type names as strings. See [Type Inference](06-type-inference.md) §Let-Generalization for details.
