# LLT Language Design

## Vision

**One language for data AND logic.** LLT is a unified data representation and transformation language. It combines the simplicity of JSON with the power of functional transformation languages like JSONnet and jq, with lazy evaluation throughout.

```
Traditional:  JSON (data) + jq (transformation) = Two languages
LLT:          LLT (data + transformation)       = One language
```

### Dual Purpose

**Data representation** — Humans and LLMs define complex data structures following composition and DRY principles. The syntax is compact and readable, with less punctuation than JSON.

**Data transformation** — The same language expresses lazy-evaluating functional transformations. There is no separate syntax for "data" vs "queries" vs "templates."

### Pipeline Model

Data flows through stages. Within a file, `---` separates independent documents. Each document's output becomes `$$` for the next:

```
file.llt
├── document 1 (data)         → $$ for doc 2
├── ---
├── document 2 (transform)    → $$ for doc 3
├── ---
└── document 3 (output)       → final value, serialized by CLI
```

Within a document, sequential expressions form a scope chain — each expression's bindings are visible to the next.

### LLM-Friendly

Designed for LLMs to generate and modify:
- Fewer tokens than JSON (no mandatory quotes on keys, no commas)
- Consistent syntax — everything is `[key: value]` or `[call $f $args]`
- Composition eliminates repetition, reducing token count further

---

## Core Principles

### Principle 1: Dicts Are Fundamental

The lowest-level unit is the dictionary (key-value pairs), not the list. First-class key-value pair syntax is core to the language.

A list is equivalent to a dict with integer keys:

```lisp
[a b c]  ≡  [0: a  1: b  2: c]
```

**Why this design:**
- **Unification** — One fundamental data structure. Functions like `map`, `filter`, `get` work uniformly on all data.
- **Flexibility** — Mixed integer and string keys naturally supported. Natural extension to keyword arguments.
- **First-class key-value pairs** — Matches the configuration language use case. Keys are names, not duplicated strings.

**Implementation:** May use different internal representations (dense vector for list-like data, HashMap for sparse/string keys) as a transparent performance optimization. Users never see the difference.

### Principle 2: One Bracket, One Structure

**`[]` is the only bracket type.** There is one syntax for the one fundamental data structure. Entries with `key:` are keyed; entries without get auto-incrementing integer keys. Both can appear in the same `[]`.

```lisp
[name: Alice  age: 30]          # All keyed — a "dict"
[a b c]                         # All auto-indexed — a "list" = [0: a  1: b  2: c]
[call $f $x timeout: 60]        # Mixed — positional + named
[]                              # Empty — list and dict are identical
```

**Parsing rule:** After parsing an entry, look ahead for `:`. If found, the entry is a key and the next thing is its value. If not, the entry is auto-indexed. The integer counter only increments for unkeyed entries — keyed entries don't consume an index.

**Positional entries must come before named entries.** Like Python's function arguments. This keeps the mapping from position to parameter unambiguous.

### Principle 3: Explicit Function Application

**`call` keyword.** No implicit head evaluation. Brackets are always data.

```lisp
[a b c]                # Data — always
[call $f $a $b $c]     # Function call — $f is the function, $a $b $c are arguments
```

Syntactically, `[call $f $x]` is a bracket expression with unkeyed entries (the same parsing mechanism as `[a b c]`). The `call` keyword triggers special-form recognition: the parser interprets the remaining entries as function + arguments, not as data. The AST represents this as a `Call` node with `func`, `args`, and `named_args` — not as a dict.

**Why:** Enables full lazy evaluation. Without `call`, the evaluator must eagerly materialize the head of every bracketed expression. With `call`, the entire application (including the function) can remain a thunk until materialized.

**Parser recognition:** The parser checks the first entry of every `[]`. If it matches a keyword (`call`, `fn`, `type`), the parser emits a specialized AST node. Otherwise it emits a `Dict` node. This is a parser-level decision, not an evaluator-level one.

```lisp
[call $f $x $y]                # Parsed as CallExpr — requires exact arity
[fn [x] [call $+ $x 1]]       # Parsed as FnExpr — function definition
```

**Edge cases:**
- `[call: something]` — the `:` makes `call` a key, not a keyword. Parsed as `Dict`.
- `$call` — a variable reference, not the keyword. `[$call $x]` is a `Dict`, not a `CallExpr`.

**No built-in alias.** Users can define their own shorthand in stdlib or user code.

### Principle 4: Lazy Evaluation

Everything is a thunk until materialized. Compute only what's needed, when it's needed.

```lisp
[
    # Won't run unless `result` is actually used
    result: [call $expensive-computation $data]

    # Infinite sequences -- only compute what you take
    naturals: [call $range 0]
    first-ten-evens: [call $collect
        [call $take 10
            [call $filter [fn [n] [call $= 0 [call $mod $n 2]]] $naturals]]]

    # Short-circuit: if condition is true, never evaluate the else branch
    value: [call $if $condition $cheap-option $very-expensive-option]
]
```

### Principle 5: Composition Over Duplication

Build complex things from simple things. No repetition.

```lisp
[
    base: [timeout: 30  retries: 3]
    dev:  [call $merge $base [env: dev]]
    prod: [call $merge $base [env: prod  timeout: 60]]
]
```

Compare to JSON where every field must be repeated:
```json
{
  "dev":  {"timeout": 30, "retries": 3, "env": "dev"},
  "prod": {"timeout": 60, "retries": 3, "env": "prod"}
}
```

---

## Confirmed Decisions

### Function Arguments

**Positional first, then named.** Like Python.

```lisp
[call $fetch "https://example.com" timeout: 30  retries: 3]
```

### Heterogeneous Keys

**Allowed by default.** Integer and string keys can coexist in the same dict. Quoted strings are valid as keys, allowing keys with spaces or special characters: `["my key": value  "another:key": 42]`.

**Computed keys and the type checker:** Dict keys can be variable references (`[$k: value]`). The evaluator resolves computed keys at runtime. The type checker resolves them at compile time via literal types: if `$k` has type `StringLiteral("name")`, the field name is `"name"`. If the type is not a literal (e.g., plain `String`), the field is excluded from the Record type. See "Literal types enable computed key resolution" in the Type System section.

### Two Map Variants

- `map` — transforms values, preserves keys
- `map-entries` — transforms (key, value) pairs, can change keys

### Comments

**`#` to end of line.** Python/shell style. No block comments.

```lisp
# This is a comment
[x: 5  y: 10]  # Inline comment
```

### Variable References

**`$` sigils.** Bare words are always string literals. `$word` is always a variable reference. This applies uniformly — no positional rules, no special cases.

```lisp
[
    name: Alice                  # key "name", value is string "Alice"
    greeting: [call $str "Hello " $name]  # $name references the binding → "Alice"
    $computed-key: some-value    # key is a reference (computed), value is string
]
```

**Why `$`:**
- Dict keys can be references too — `[$key: $value]` — no special syntax needed for computed keys
- `[name: $name]` is visually clear: key "name" gets the *value* of `name`
- Functions are values: `[call $map ...]` makes it obvious `map` is a reference being looked up, not a keyword
- Bare strings don't need quotes: `[env: production]` just works
- Synergy with string interpolation (if added): `"Hello $name"`

**No special case for `call`.** The function position uses `$` like any other reference. This reinforces that functions are regular values.

### Literal Recognition

**The tokenizer recognizes literals by pattern, not the evaluator.** This is consistent with parser-level special form recognition — the distinction between literal types is made at tokenization time, before any evaluation occurs.

**Precedence order** for classifying a bare token:

1. **`$` sigil** — if the token starts with `$`, it's a variable reference. Always.
2. **Numeric literal** — a token matching `[0-9]+` is an `Int` literal. A token matching `[0-9]+\.[0-9]+` is a `Float` literal. Recognized by the tokenizer before bare-word rules apply.
3. **Boolean literal** — the bare words `true` and `false` are recognized by the tokenizer as `Bool` literals. They are NOT variable references and NOT strings.
4. **Bare word string** — everything else (unless it starts with `"`, `[`, `@`, etc.) is a string literal.

**Quoting forces string interpretation.** `"true"` is the string `"true"`, `"42"` is the string `"42"`. Quoting is the escape hatch from literal recognition.

```lisp
42              # Int literal
3.14            # Float literal
true            # Bool literal
false           # Bool literal
hello           # String "hello"
"true"          # String "true" (quoting overrides)
"42"            # String "42"
$true           # Variable reference named "true"
```

**Why tokenizer-level:** If `true` and `42` were bare-word strings that the evaluator later reinterpreted, it would break the "bare words are always strings" rule in confusing ways — `hello` would be a string but `true` would secretly be a boolean. By having the tokenizer recognize these patterns first, the rule becomes precise: bare words that don't match any prior pattern (sigil, numeric, boolean) are strings.

### Threading `->` in Stdlib

Not language syntax. Implemented in stdlib:

```lisp
->: [fn [data ...stages]
    [call $reduce [fn [acc f] [call $f $acc]] $data $stages]]
```

### Type System

**Mandatory, Haskell-style, fully inferred.** Every value has a type. Row polymorphism for dicts. Type errors raised early — good for LLMs and LSP feedback.

**Annotations are optional but enforced.** The compiler infers types when annotations are omitted. If you write an annotation, it's a contract — the compiler checks the actual type matches and raises an error on mismatch.

### `@` Property Annotations

**`@` attaches a property dict** to a parameter or function. Shorthand: bare word after `@` means `[type: BareWord]`.

```lisp
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

**A parameter with `default:` is a named argument.** No default = positional/required.

```lisp
fetch: [fn@String [url@String
                   timeout@[type: Number  default: 30]
                   retries@[type: Number  default: 3]]
    ...]

# Call with bare key-value named args
[call $fetch "https://example.com" timeout: 60]
# $url = "https://example.com", $timeout = 60, $retries = 3 (default)
```

**Named args at the call site** are bare `key: value` pairs inside `[call ...]`. This is natural — the call expression is a dict, with integer-keyed entries for positional args and string-keyed entries for named args.

### `@` on Expressions — Type Assertions

**`[@Type $expr]` is a type assertion expression.** Materializes the value, checks its type, throws on mismatch. No `as` keyword needed — `@` handles it.

```lisp
data: [call $from-json $input]        # type: Any

# Type assertion — throws if wrong
name: [@String $data.name]

# Inline in a call
[call $+ [@Number $x] 1]

# Complex type
users: [@[Person] [call $from-json $input]]
```

**With property dict — safe cast with fallback:**

```lisp
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

### Return Type on `fn@`

**`fn@Type` declares the return type.** Optional — inferred if omitted. Enforced if specified.

```lisp
# Return type annotated — compiler checks body matches
double: [fn@Number [x@Number] [call $* $x 2]]

# Return type omitted — compiler infers Number
double: [fn [x@Number] [call $* $x 2]]

# Wrong return type — compile error
double: [fn@String [x@Number] [call $* $x 2]]    # Error: body returns Number, not String
```

**`Fn@Return [Params]` for function types.** Function type expressions mirror function definitions:

```lisp
# Definition:  fn@Return [params] body
[fn@Number [x@Number y@Number] [call $+ $x $y]]

# Type:        Fn@Return [ParamTypes]
[Fn@Number [Number Number]]
```

`Fn` is uppercase (concrete type constructor convention). The return type attaches via `@`, matching `fn@Type`. Parameter types go in brackets, matching the param list in definitions. All types must be specified — there is no body to infer from.

```lisp
[Fn@b [a]]              # function from a to b
[Fn@Bool [a]]           # predicate: a to Bool
[Fn@c [a b]]            # two-arg function: a, b to c
[Fn@[Fn@c [b]] [a]]    # higher-order: a to (b to c)
```

**`...` for open records** (row polymorphism):

```lisp
# Open — at least these keys, possibly more
[name: String ...]

# Closed — exactly these keys
[name: String  age: Number]

# Named row variable (advanced)
[name: String ...rest]
```

**Type aliases via `[type ...]`** — textual expansion with free variables connecting by name:

```lisp
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

**Literal types.** Integer and string literals carry their value in the type: `42` has type `IntLiteral(42)`, `"hello"` has type `StringLiteral("hello")`. Literal types are subtypes of their base types: `IntLiteral(n)` <: `Int` <: `Number`, `StringLiteral(s)` <: `String`. All bindings in LLT are immutable, so literal types never widen implicitly -- they widen only when an annotation demands the base type. Float and Bool literals do not need literal type variants because they cannot be used as dict keys.

**Literal types enable computed key resolution.** When a dict has a computed key like `[$k: 42]`, the type checker infers the type of `$k` in the parent scope. If it resolves to a literal type (e.g., `StringLiteral("name")`), the type checker extracts the literal value and uses it as the field name. If the key expression resolves to a non-literal type (e.g., `String`) or `Any`, the type checker cannot determine the field name statically -- the entry's value is still type-checked, but the field is excluded from the Record type. This is the conservative correct behavior: the Record only contains fields whose names are statically known.

```lisp
[k: hello  $k: 42]       # type: [k: StringLiteral("hello")  hello: IntLiteral(42)]
                          # $k resolves to StringLiteral("hello") → field name "hello"

[k: hello]
[$k: 42]                  # scope chain: $k resolves from parent → field "hello"

[k: $dynamic  $k: 42]    # $k has type String (not literal) → field excluded from Record
```

**Dict values are never type-annotated.** Always inferred from literals/expressions.

**Type inference for letrec dicts:** Dict entries form a letrec scope where all keys are visible to all values. The type checker handles this in four passes: (0) resolve key names -- literal keys extracted directly, computed keys resolved via type inference in parent scope, (1) bind all resolved key names to `Any`, (2) register type aliases sequentially (each sees previously registered siblings), (3) infer actual value types. Forward references resolve to `Any`. Polymorphic function calls use Hindley-Milner unification: each call site instantiates fresh type variables, unification binds them against argument types, and the substitution is applied to determine the return type. Computed keys whose type is not a literal are excluded from the Record's fields but their values are still type-checked.

**Type alias entries are excluded from record fields.** A `[type ...]` entry registers an alias in the type environment but does not contribute a field to the enclosing record's type. This matches the evaluator, which returns an empty dict for type alias entries.

**Function type param lists:** `[Fn@Return [ParamTypes]]` is the full function type syntax. The type checker resolves both the return type annotation and parameter type list, producing `Type::Function { params, ret }`. All types in the param list must be specified explicitly.

### Exceptions by Default, `$try` in Stdlib

**Errors are exceptions that propagate when a thunk is materialized.** No `Result` wrapping in normal code. Thunks record source location at creation for error reporting.

```lisp
[
    x: [call $/ 1 0]              # Thunk created — no error yet
    y: [call $+ $x 1]             # Materializing $x raises: "division by zero"
    z: 42                          # Fine — $x never materialized through $z
]

# Explicit catching via stdlib
safe: [call $try [fn [] [call $/ 10 2]]]       # → [ok: 5]
safe: [call $try [fn [] [call $/ 1 0]]]        # → [err: "division by zero"]
safe: [call $try-or [fn [] [call $/ 1 0]] 0]   # → 0
```

**`$try` return shape:** `$try` returns a tagged dict — `[ok: value]` on success or `[err: message]` on failure. This is an ordinary dict, not a special type. Pattern match on the key to distinguish outcomes.

**Why:** Simple default path — most code lets errors propagate. Lazy eval means unmaterialized errors never happen ("pay for what you use"). `$try` available when explicit handling is needed.

**Implementation note:** Thunks must record definition-site source location. When materialized, the materialization-site span is passed as a parameter to `materialize()`, not stored in the thunk. Error messages include both locations and a reconstructed call stack showing the chain of materializations. The evaluator depth limit (256) counts nesting depth of evaluation calls, not total operations — deeply nested function calls hit the limit, but a linear chain of thunks does not.

### No `defn` — Functions Are Dict Entries

**No `defn` special form.** Named functions are ordinary dict entries using `fn`:

```lisp
[
    double: [fn@Number [x@Number] [call $* $x 2]]
    add: [fn@Number [x@Number y@Number] [call $+ $x $y]]
]
```

**Why:** Consistent with dict-first design. Every binding is a key-value pair, no exceptions. Fewer special forms to implement.

### Variadic Parameters with `...`

**`...name` collects remaining arguments.** Consistent with `...` in type annotations for open records.

```lisp
->: [fn [data ...stages]
    [call $reduce [fn [acc f] [call $f $acc]] $data $stages]]

# Called as:
[call $-> $data $step1 $step2 $step3]
# $data = ..., $stages = [$step1 $step2 $step3]
```

### Insertion Order

**Dicts preserve insertion order for iteration and display.** Semantically, entry order doesn't matter (letrec scoping). But iteration via `$keys`, `$values`, `$map` etc. follows the order entries appear in source. `$merge` preserves left order, appends new keys from right.

### Recursive Dict Scoping (`letrec`)

**All entries in a dict see each other.** Entry order doesn't matter semantically.

```lisp
[
    x: [call $+ $y 1]    # thunk — when materialized, looks up $y → 6
    y: 5
]

# Mutual recursion works
[
    even?: [fn [n] [call $if [call $= $n 0] true  [call $odd?  [call $- $n 1]]]]
    odd?:  [fn [n] [call $if [call $= $n 0] false [call $even? [call $- $n 1]]]]
]
```

**Why:** Dicts are the fundamental unit — they shouldn't be order-dependent. Lazy evaluation makes this free: all bindings are thunks referencing a shared environment. This matches Haskell's `let`/`where` and Nix's attribute sets.

**Key evaluation scope:** Dict keys are evaluated in the *parent* scope, not the dict's own letrec scope. This means key expressions cannot reference sibling bindings within the same dict. This is intentional for letrec correctness: keys must be deterministic regardless of entry order, and allowing keys to depend on sibling values (which are still unevaluated thunks) would introduce order-dependence or require eager evaluation of referenced entries.

**Circular dependencies** are detected at materialization-time and reported with a clear cycle trace.

**Nested dicts create new scopes.** Each `[]` dict introduces a new lexical scope. Inner scopes see all bindings from outer scopes, and inner bindings shadow outer bindings of the same name within that inner dict. Scoping is lexical, not dynamic — closures capture their defining environment, not the calling environment. This matches Haskell's `let`/`where` and Nix's attribute sets.

The `Environment` struct's `parent` field implements this: each nested dict gets a new `Environment` whose `parent` points to the enclosing dict's environment. Variable lookup walks the parent chain outward.

```lisp
[
    x: 10
    inner: [
        x: 20              # shadows outer x
        y: [call $+ $x 1]  # $x is 20 (inner), not 10 (outer)
    ]
    z: [call $+ $x 1]      # $x is 10 (outer)
]
```

### Duplicate Keys Are Errors

**Duplicate keys in dict literals are an error.** Use `$merge` for intentional overrides.

```lisp
[name: Alice  name: Bob]              # → Error: duplicate key "name"
[call $merge [name: Alice] [name: Bob]]  # → [name: Bob]  (right-biased, intentional)
```

**Why:** Duplicate keys + lazy evaluation creates confusing semantics — depending on the scoping model, derived values may see different bindings of the same key. Prohibiting duplicates eliminates the ambiguity entirely and catches copy-paste errors.

### Data Access — Two Modes

Data access has two distinct modes: **key-based** (look up by key) and **position-based** (look up by insertion-order index). For dense lists `[a b c]` = `[0: a 1: b 2: c]`, these coincide. They diverge for sparse or mutated dicts.

**Key-based access** — brackets and dot notation:

```lisp
# Dot notation (string keys)
$person.name                    # → [call $get $person name]
$config.database.host           # → chained $get

# Bracket notation (any key type)
$data[5]                        # Integer key 5
$data[-1]                       # Integer key -1 (NOT last element)
$data[$key]                     # Computed key lookup
$config.services[0].host        # Mixed chaining — key 0 on services
```

**Rules:** Only `$ref.key` / `$ref[key]` — the left side must start with `$`. Bare `foo.bar` is just a string containing a dot. Brackets are always key-based — `$data[5]` finds the entry whose key is 5, not the 5th entry by position.

**Key-range slicing** with `..`:

```lisp
$data[2..5]                     # Entries with keys in [2, 5)
$data[2..]                      # Entries with keys ≥ 2
$data[..3]                      # Entries with keys < 3
```

Key-range slicing requires keys to be comparable. All-integer or all-string keys work; mixed-type keys are an error (caught by the type system). The range operator uses `..` (not `:`, which would conflict with the key-value separator).

**Position-based access** — stdlib functions:

```lisp
[call $nth $data 0]       # First entry (position 0)
[call $nth $data -1]      # Last entry (negative = from end)
[call $last $data]              # Last entry (alias)
[call $slice $data 2 5]         # Entries at positions 2, 3, 4
```

**Why the split:** Position-based access on a dict that has been mutated over time has less-than-useful ordering. Making it a function call (not syntax) signals that it's the unusual operation. For the common case of dense lists, `$data[0]` (key 0) and `[call $nth $data 0]` (position 0) return the same thing — you never need `$nth` unless you specifically want insertion-order semantics on sparse data.

### List vs Dict Operations — Renumbering Rule

**List operations require integer keys and always produce dense `[0..n]`.** Error on string keys. Dict operations preserve keys. Universal operations work on both and preserve keys.

```lisp
# List operations — integer keys only, always renumber
[call $first [alice bob carol]]         # → alice
[call $rest [alice bob carol]]          # → [bob carol] = [0: bob  1: carol]
[call $cons z [a b c]]                  # → [z a b c] = [0: z  1: a  2: b  3: c]
[call $conj [a b c] d]                  # → [a b c d] = [0: a  1: b  2: c  3: d]
[call $concat [a b] [c d]]             # → [a b c d] = [0: a  1: b  2: c  3: d]
[call $reverse [a b c]]                 # → [c b a] = [0: c  1: b  2: a]
[call $sort [cherry apple banana]]      # → [apple banana cherry] — sorts by value, discards original keys
[call $reindex [0: a  5: b  10: c]]     # → [a b c] = [0: a  1: b  2: c]
```

**Why this split:**
- No ambiguity about which operations renumber — it's determined by the category, not the data
- List operations always give you clean, predictable lists
- Dict operations never silently destroy your key structure
- `$filter` is universal (preserves keys), so filtering a list can produce a sparse result — pipe through `$reindex` if you want dense keys back
- The type system enforces the boundary: list operations require `[a]` (integer-keyed)

```lisp
# $filter preserves keys (universal operation)
$data: [alice bob carol dave]
[call $filter [fn [x] [call $not [call $= $x bob]]] $data]
# → [0: alice  2: carol  3: dave]    sparse — key 1 gone

# Pipe through $reindex for a clean list
[call $-> $data
    [call $filter [call $not [call $= $_ bob]] $_]
    $reindex]
# → [alice carol dave] = [0: alice  1: carol  2: dave]

# $filter on string-keyed dicts preserves keys (obviously)
[call $filter [fn [v] [call $> $v 0]] [x: 1  y: -2  z: 3]]
# → [x: 1  z: 3]
```

**`$conj` on sparse data:** `$conj` delegates to `$append`, which uses the maximum existing integer key + 1 as the new key (or 0 if no integer keys exist). This avoids key collisions even on sparse data:

```lisp
# Dense list — $conj works as expected
[call $conj [a b c] d]                  # → [0: a  1: b  2: c  3: d]

# Sparse data — no collision, key 11 is used (max 10 + 1)
$sparse: [0: a  5: b  10: c]
[call $conj $sparse d]                  # → [0: a  5: b  10: c  11: d]
```

### No Null — Missing Keys Are Errors

**No `null` value in the language.** Accessing a nonexistent key is an error.

```lisp
[call $get $person name]         # → Alice
[call $get $person occupation]   # → Error: key "occupation" not found

# Safe alternative with default
[call $get-or $config timeout 30]  # → 30 if "timeout" is missing

# Check existence
[call $has? $config timeout]       # → true/false
```

**Why no null:**
- **Row polymorphism catches it at compile time.** A function taking `[name: String ...]` guarantees `name` exists. Most missing-key bugs never reach runtime.
- **Lazy eval provides a safety net.** `[x: [call $get $dict maybe-missing]]` doesn't error until `$x` is materialized. If you never use `$x`, no error.
- **No null confusion.** Can't confuse "key exists with null" vs "key is missing." Every key that exists has a real value.
- **Clean data representation.** Config files have no `null` noise — every key is meaningful.

**JSON null mapping:** Since LLT has no null value, `$from-json` (and CLI stdin JSON injection) maps JSON `null` to `[]` (empty dict). This means it is impossible to distinguish "was null" from "was empty object" after conversion. This is an intentional trade-off -- LLT's "no null" design prioritizes simplicity over round-trip fidelity with JSON.

### Special Forms vs Stdlib Functions

**Lazy evaluation means most "control flow" is just regular functions.** In an eager language, `if` must be a special form because both branches would be evaluated before `if` runs. In LLT, all arguments are thunks — the unused branch is never materialized.

Only constructs that affect **binding structure** or **dict construction** need to be special forms (built into the language). The parser recognizes these by checking the first entry of every `[]`:

| Language-level (special forms) | Why |
|-------------------------------|-----|
| `call` | Triggers function application (exact arity required) |
| `fn` | Introduces parameter bindings, creates a new scope |
| `type` | Compile-time type declaration, not a runtime value |

Everything else can be a regular function in the stdlib:

| Stdlib function | How it works with lazy eval |
|----------------|----------------------------|
| `if` | Materializes `cond`, returns the matching branch thunk (other branch never materialized) |
| `cond` | Materializes conditions in order, returns first matching branch |
| `when` | Like one-armed `if`; materializes condition, returns body or `[]` |
| `unless` | Inverse of `when`; materializes condition, returns body or `[]` |
| `and` | Materializes first arg; if false, returns false without materializing second |
| `or` | Materializes first arg; if true, returns true without materializing second |
| `not` | Materializes its argument; returns the boolean inverse |

```lisp
# These are stdlib functions, not special forms:
[call $if [call $> $x 0] positive non-positive]
[call $and [call $valid? $input] [call $process $input]]  # process never called if invalid
[call $or $cached-value [call $expensive-compute]]        # compute skipped if cached
```

### Single Bracket Syntax

**`[]` is the only bracket type.** No `()` in the language. Every expression uses `[]`. The `call` keyword distinguishes function application from data — the bracket type is not needed for this. See Principle 2 for the auto-indexing rule, parsing rule, and positional-before-named ordering constraint.

**Why single brackets:**
- Simpler — one bracket type, one concept
- `()` and `{}` are both freed for future use
- `call` already signals function application — `()` was redundant
- `[]` is familiar from JSON, Python, JavaScript
- True unification: there's one data structure, so there's one syntax

**Parser complexity trade-off:** Single brackets with overloaded semantics require careful disambiguation: keyword recognition (`call`/`fn`/`type` vs dict entries), access chain whitespace sensitivity (`$a.b` vs `$a .b`), and special-form parsing. This complexity is concentrated in the parser — the evaluator and user-facing syntax remain simple.

### Numeric Types — `Int`, `Float`, `Number`

**Two concrete types: `Int(i64)` and `Float(f64)`.** `Number` is the supertype that accepts either. Integer literals carry their value: `42` has type `IntLiteral(42)`, which is a subtype of `Int`. Float literals do not have a literal type variant because floats cannot be dict keys.

```lisp
port: 8080                      # Int — no decimal point
pi: 3.14                        # Float — has decimal point
x@Int                           # must be an integer
y@Float                         # must be a float
z@Number                        # accepts either
```

**Arithmetic auto-promotes.** The compiler handles promotion with a fixed table — no typeclasses needed:

| Left | Op | Right | Result |
|------|-----|-------|--------|
| Int | `$+`, `$-`, `$*` | Int | Int |
| Int | any | Float | Float |
| Float | any | Int | Float |
| Float | any | Float | Float |
| any | `$/` | any | Float (always) |
| Int | `$quot`, `$mod` | Int | Int |

```lisp
[call $+ 5 3]                   # → 8 (Int)
[call $+ 5 3.0]                 # → 8.0 (Float)
[call $/ 10 3]                  # → 3.333... (Float — $/ always returns Float)
[call $quot 10 3]               # → 3 (Int — truncated integer division, prelude function using $trunc)
[call $mod 10 3]                # → 1 (Int — remainder)
```

**Integer arithmetic uses checked semantics.** `Int` operations (`$+`, `$-`, `$*`) use Rust's `checked_add`/`checked_sub`/`checked_mul`, so overflow returns an error rather than wrapping or panicking. This prevents silent data corruption on large values. Width-specific types like `Int32` could enforce narrower range constraints via the contracts system.

**Dict key integration:** `Int` values are directly usable as dict keys. `Float` values cannot be used as keys (deferred — precision issues).

**Width-specific types** (`Int32`, `Int64`, `Int128`, `Decimal`, etc.) are deferred to the contracts system. These are range constraints on `Int`/`Float`, not new runtime representations. `Decimal` (if ever needed) would require a new Value variant.

**Typeclasses deferred.** The promotion table is hardcoded in the compiler. If extensibility is needed later (e.g., user-defined numeric types), typeclasses can generalize the mechanism without breaking existing code.

### Special Form Recognition

**The parser recognizes special forms by keyword.** When the first entry of a `[]` is a bare word matching `call`, `fn`, or `type`, the parser emits a specialized AST node. Otherwise it emits a `Dict` node.

```lisp
[call $f $x]          # CallExpr — first entry is bare word "call"
[fn [x] $x]           # FnExpr
[type [Fn@b [a]]]     # TypeExpr

[call: something]     # Dict — "call" followed by ":" is a key, not a keyword
[$call $x $y]         # Dict — $call is a variable reference, not the keyword
[mycall $f $x]        # Dict — "mycall" is not a recognized keyword
```

**Why parser-level:** The distinction between special forms and data must be unambiguous before evaluation. If deferred to the evaluator, `[call $f $x]` would first be constructed as a dict `[0: call  1: $f  2: $x]`, then the evaluator would need to inspect key 0 — but at that point the dict is already a thunk and the string `"call"` is indistinguishable from user data that happens to contain the word "call". Parser-level recognition avoids this ambiguity entirely.

### Tokenization Rules

Rules for how the tokenizer/parser handles `$`, `.`, `[`, `..`, and `@`. These characters have context-dependent meaning and require careful disambiguation.

#### `$` Sigil — Variable References

`$` starts a variable reference token. The tokenizer reads the longest valid identifier after `$` using a denylist approach — any character is valid in an identifier except structural delimiters:

**Excluded from identifiers:** whitespace, `[`, `]`, `:`, `;`, `#`, `"`, `@`, `.`

Everything else is a valid identifier character, including `$` itself, digits, unicode, and operator symbols. This means `$$` is VarRef("$") (the inter-document pipeline variable), `$$foo` is VarRef("$foo"), and `$0` is VarRef("0").

**Denylist rationale:** The denylist approach (allow-by-default, exclude structural delimiters) provides extensibility for new operators without reserved keywords, and enables full Unicode identifier support (emoji, non-Latin scripts) without explicit allow-lists.

The token ends at the first excluded character. `.` and `[` are **not** part of the variable name — they are separate access operators that the parser chains onto the reference.

```lisp
$name                    # Token: VarRef("name")
$has?                    # Token: VarRef("has?")
$my-var                  # Token: VarRef("my-var")
$_private                # Token: VarRef("_private")
$$                       # Token: VarRef("$") — inter-document pipeline
$$foo                    # Token: VarRef("$foo")
$0                       # Token: VarRef("0")
$data.name               # Tokens: VarRef("data"), Dot, BareWord("name")
$data[0]                 # Tokens: VarRef("data"), BracketAccess, Int(0), CloseBracket
```

A bare `$` not followed by any valid identifier character is a tokenizer error.

#### `.` Dot Access

`.` immediately after a `$ref` token or after a `]` closing a bracket access (no whitespace) triggers dot-access parsing. The parser reads the next bare word as the key name.

`.` with whitespace before it is part of a bare word string — it has no special meaning.

```lisp
$person.name             # Dot access: get key "name" from $person
$config.database.host    # Chained dot access: $config → "database" → "host"
$data[0].name            # Dot access after bracket access

some.file.txt            # Bare word string: "some.file.txt" (no $ prefix)
$x . y                   # $x is a VarRef, ". y" is not dot access (whitespace before .)
```

| Input | Tokens | Interpretation |
|-------|--------|----------------|
| `$a.b` | `VarRef("a")`, `Dot`, `BareWord("b")` | Dot access |
| `a.b` | `BareWord("a.b")` | String containing a dot |
| `$a .b` | `VarRef("a")`, `BareWord(".b")` | VarRef then separate string |
| `$a[0].b` | `VarRef("a")`, `BracketAccess(0)`, `Dot`, `BareWord("b")` | Bracket then dot |

#### `[` Bracket Access

`[` immediately after a `$ref` token or after `.key` or after `]` (no whitespace) triggers bracket-access parsing. The tokenizer reads the contents as an expression (variable ref, integer, or range) up to the matching `]`.

`[` with whitespace before it starts a new nested `[]` expression (a dict/list/call).

```lisp
$data[5]                 # Bracket access: key 5 on $data
$data[$key]              # Bracket access: computed key
$data[2..5]              # Bracket access with range
$config.services[0].host # Bracket access in a chain

$data [5]                # Two separate things: VarRef("data"), then list [0: 5]
```

| Input | Tokens | Interpretation |
|-------|--------|----------------|
| `$a[0]` | `VarRef("a")`, `BracketAccess(0)` | Bracket access |
| `$a [0]` | `VarRef("a")`, `OpenBracket`, `Int(0)`, `CloseBracket` | VarRef then new list |
| `$a[0][1]` | `VarRef("a")`, `BracketAccess(0)`, `BracketAccess(1)` | Chained bracket access |
| `$a.b[0]` | `VarRef("a")`, `Dot`, `BareWord("b")`, `BracketAccess(0)` | Dot then bracket |

#### `..` Range Operator

Inside bracket access (`$data[2..5]`), two consecutive dots form the range operator. The tokenizer recognizes `..` only in the bracket-access context. Outside bracket access, `..` is literal — it is part of a bare word string.

```lisp
$data[2..5]              # Range: keys in [2, 5)
$data[2..]               # Range: keys >= 2
$data[..3]               # Range: keys < 3

config..bak              # Bare word string: "config..bak"
path/to/../file          # Bare word string: "path/to/../file"
```

| Input | Context | Interpretation |
|-------|---------|----------------|
| `$data[2..5]` | Inside bracket access | Range operator: keys 2 to 5 |
| `$data[..]` | Inside bracket access | Range operator: all keys |
| `file..bak` | Bare word | String: "file..bak" |
| `a..b` | Bare word | String: "a..b" |

#### `@` Annotation

**`@` is always a structural separator.** It is not a valid bare word character. Wherever `@` appears immediately after a bare word (no whitespace), it separates the word from an annotation value.

This applies uniformly:

1. **Parameter annotation** — `x@Number` in a param list
2. **Return type annotation** — `fn@Number` on function definitions
3. **Value annotation** — `Fn@Number` in any value position (e.g., type constructors)
4. **Type assertion** — `[@Type $expr]` at the start of a bracket expression

```lisp
# Parameter context
x@Number                 # param "x" with annotation Number
timeout@[type: Number]   # param "timeout" with property dict annotation

# Function return type
fn@String                # function returning String

# Value context (generalized annotation)
Fn@Number                # annotated value: "Fn" with annotation Number
Fn@[Fn@c [b]]           # nested: function returning a function type

# Type assertion
[@Number $expr]          # assert $expr is Number

# Strings containing @ must be quoted
"email@example.com"      # quoted string
```

| Input | Interpretation |
|-------|----------------|
| `x@Number` | Annotation: "x" with type Number |
| `fn@String` | fn with return annotation String |
| `Fn@b` | Annotated value: "Fn" annotated with "b" |
| `[@String $x]` | Type assertion expression |
| `"a@b"` | Quoted string "a@b" |

#### Bare Word Character Set

Bare words are unquoted string literals. They follow these rules:

**Valid characters (denylist approach):** Like variable identifiers (see [Tokenization Rules](#tokenization-rules)), bare words use a denylist — any character is valid *except* structural delimiters: whitespace, `[`, `]`, `:`, `;`, `#`, `"`, `@`, and `$`. This means `[a-zA-Z0-9_/.-]`, Unicode, and most other characters are all valid in bare words. Bare words are the default — anything that isn't a recognized special token is a bare word.

**Cannot start with:** `$`, `@`, `#`, `[`, `]`, `:`, `;`, `"`, or `...` (variadic sigil). These characters have special meaning at the start of a token.

**Terminators — bare words end at the first structural delimiter:**

| Character | Effect |
|-----------|--------|
| Whitespace | Ends the bare word |
| `[` | Ends the bare word (starts bracket access or new expression) |
| `]` | Ends the bare word (closes enclosing expression) |
| `:` | Ends the bare word (key-value separator) |
| `;` | Ends the bare word (entry separator) |
| `#` | Ends the bare word (starts comment) |
| `@` | Ends the bare word (starts annotation) |
| `$` | Ends the bare word (starts variable reference) |

```lisp
hello                    # Bare word: "hello"
some.file.txt            # Bare word: "some.file.txt"
path/to/file             # Bare word: "path/to/file"
my-key                   # Bare word: "my-key"
config..bak              # Bare word: "config..bak"
日本語                    # Bare word: "日本語" (Unicode)

# These are NOT bare words — first character is special
$name                    # Variable reference (starts with $)
"has spaces"             # Quoted string (starts with ")
#comment                 # Comment (starts with #)
[list]                   # Bracketed expression (starts with [)
```

### Value Boundary Rules

**Every entry's value is exactly one token or one `[]` expression.** There are no multi-value entries. Whitespace separates entries — after parsing a key's value (one token or one `[]`), the next whitespace-separated token is the start of a new entry.

```lisp
[name: Alice age: 30]           # Two key-value pairs: name→Alice, age→30
[key: [a b c]]                  # One key-value pair: key→[a b c] (nested [] is a single value)
[call $f $x $y]                 # Function call — $f is the function, $x and $y are arguments
[x: 1 y]                       # ERROR — positional entry after named entry
```

**Nested `[]` counts as a single value.** When a key's value starts with `[`, the parser consumes the entire balanced bracket expression as that key's value:

```lisp
[config: [timeout: 30 retries: 3]]   # config → the entire inner dict
[steps: [a b c]]                      # steps → the list [a b c]
```

The parser treats `[key: value1 value2 value3]` such that `key` has value `value1`, while `value2` and `value3` become separate auto-indexed entries. Multi-value semantics are achieved by wrapping in `[]`:

```lisp
# Old (removed): key has multi-value [value1 value2 value3]
[key: value1 value2 value3]

# New: key has value "value1"; "value2" and "value3" are auto-indexed entries
# Equivalent to: [key: value1  0: value2  1: value3]

# To associate multiple values with a key, wrap them in []:
[key: [value1 value2 value3]]
```

**Why:** One-token-per-value eliminates ambiguity about where one entry ends and the next begins. The parser never has to guess whether a bare word belongs to the previous entry's value or starts a new entry. Combined with the positional-before-named ordering constraint, every token's role is unambiguous from left to right.

### No Auto-Curry — Lambdas and `$_` Shorthand

**`call` requires exact arity.** Passing too few or too many arguments is an error. Use lambdas or `$_` shorthand to adapt arity.

```lisp
add: [fn@Number [x@Number y@Number] [call $+ $x $y]]

[call $add 1 2]                # → 3 (exact arity)
[call $add 1]                  # ERROR: $add expects 2 arguments, got 1
```

**`$_` implicit lambda shorthand:** Any `[...]` expression that directly contains `$_` (not nested inside an inner `[...]`) is automatically wrapped in a single-argument function. `$_` becomes the parameter. All occurrences of `$_` in that bracket refer to the same parameter.

```lisp
[call $add $_ 1]               # → [fn [_] [call $add $_ 1]]
[call $> $_.age 30]            # → [fn [_] [call $> $_.age 30]]
$_.name                        # → [fn [_] $_.name]  (access chain, no brackets)
```

**`$_` in dict values:** `$_` desugaring also applies to dict literals. If any entry value directly contains `$_`, the entire dict is wrapped in an implicit lambda:

```lisp
[name: $_.name  age: $_.age]   # → [fn [_] [name: $_.name  age: $_.age]]
```

This is useful for creating projection functions in pipelines:

```lisp
[call $map [name: $_.name  age: $_.age] $users]
```

**`$_` in func position:** `$_` in the function position of `[call $_ ...]` does **not** trigger implicit lambda desugaring. Only `$_` in arguments, named arguments, dict values, and access chains triggers desugaring. `[call $_ $x]` is a call where the function is looked up from the variable `_`, not an implicit lambda.

**`$_` in access chain keys/bounds:** `$_` in the key position of bracket access (e.g., `$data[$_]`) or in range bounds (e.g., `$data[$_..5]`) does **not** trigger desugaring. Only `$_` as the *target* of an access chain (e.g., `$_[0]`, `$_.name`) triggers implicit lambda wrapping.

**Scoping rule:** The lambda boundary is the innermost `[...]` that directly contains `$_`. Nested bracket expressions that contain their own `$_` create separate lambdas:

```lisp
[call $filter [call $> $_.age 30] $users]
#            └─── inner $_ ───┘
# Inner [call $> $_.age 30] contains $_ → becomes [fn [_] [call $> $_.age 30]]
# Outer [call $filter ...] does NOT contain $_ directly → stays as-is
# Result: [call $filter [fn [_] [call $> $_.age 30]] $users]
```

**Pipeline interaction:** `$->` threads a value through a list of single-argument functions. Each pipeline step is either a function reference (for 1-arg functions) or a `$_` expression that creates an implicit lambda:

```lisp
[call $-> $data.users
    [call $filter [call $> $_.age 30] $_]   # two $_ levels: inner = element, outer = collection
    [call $map $_.name $_]                  # inner $_.name = element transform, outer $_ = collection
    $sort]                                  # ref: already 1-arg
```

Desugaring of `[call $filter [call $> $_.age 30] $_]`:
1. Inner `[call $> $_.age 30]` contains `$_` → `[fn [_] [call $> $_.age 30]]`
2. Outer `[call $filter ... $_]` still contains `$_` → `[fn [_] [call $filter [fn [_] [call $> $_.age 30]] $_]]`
3. Each `$_` binds to its innermost enclosing lambda (lexical scoping)

**`$apply` spreads a list into function arguments:**

```lisp
args: [5 10]
[call $apply $+ $args]         # → [call $+ 5 10] → 15
```

**Why not auto-curry:** Auto-currying makes arity errors silent. Pass too few arguments and you get a partial application instead of an error. Explicit arity checking catches mistakes.

### Document Structure

An LLT **file** contains one or more **documents** separated by `---`. Each document contains one or more **expressions**. This three-level hierarchy governs scoping, isolation, and data flow.

```
file
├── document 1
│   ├── expression 1  (e.g., [call $include "utils.llt"])
│   ├── expression 2  (e.g., [x: 10  double: [fn [n] [call $* $n 2]]])
│   └── expression 3  (e.g., [result: [call $double $x]])
├── ---
└── document 2
    ├── expression 1
    └── expression 2
```

#### Within a Document: Scope Chains

LLT has one scoping mechanism -- lexical scope with parent chains -- applied at two levels:

1. **Within a dict (letrec):** All entries in a single `[...]` share one environment. Entries can reference each other regardless of order, including mutual recursion. This is the same as Haskell's `let`/`where` or OCaml's `let rec`.

2. **Between sequential expressions:** Each expression's result dict becomes the parent scope for the next expression. Names from earlier expressions are visible but can be shadowed. Only string-keyed entries become named bindings in the scope chain; int-keyed entries remain accessible via bracket access on the result but do not introduce variable bindings. This is analogous to a sequence of `let` blocks in ML-family languages, or nested `letrec` in Scheme.

These are not two different mechanisms. They are the same parent-chain lookup applied at different granularities. Variable lookup always walks the parent chain until it finds a match.

```
Builtins ($+, $eval, $if, ...)
  └── Expression 1's dict (letrec within)
        └── Expression 2's dict (letrec within, sees Expr 1 via parent)
              └── Nested inner dict (sees Expr 2 via parent, Expr 1 via grandparent)
```

**Letrec within a dict:**

```lisp
[
  x: 10
  double: [fn [n] [call $* $n 2]]
  y: [call $double $x]            # sees $x and $double (same dict, letrec)
]
```

All entries share one environment. Order of definition does not matter -- `y` can reference `double` even if `double` appeared after `y` in the source. This enables mutual recursion:

```lisp
[
  even?: [fn [n] [call $if [call $= $n 0] true  [call $odd?  [call $- $n 1]]]]
  odd?:  [fn [n] [call $if [call $= $n 0] false [call $even? [call $- $n 1]]]]
]
```

**Sequential expressions (scope chain):**

```lisp
# Expression 1: establishes bindings
[
  x: 10
  double: [fn [n] [call $* $n 2]]
]

# Expression 2: sees Expression 1's bindings via parent scope
[
  y: [call $double $x]    # $x and $double visible from parent
  x: 20                   # shadows Expression 1's x
  z: [call $+ $x $y]      # $x is 20 (local letrec), $y is 20
]
```

Expression 2 creates a fresh letrec environment with Expression 1's environment as its parent. Within Expression 2, `$x` resolves to the local binding (20), not Expression 1's binding (10). `$double` is found by walking up to the parent.

**Nested dicts (lexical scope):**

Inner dicts see enclosing dicts' bindings by walking the parent chain. Siblings in a parent dict share one environment (letrec), so lateral access is free:

```lisp
[
  db: [host: localhost  port: 5432]
  cache: [
    host: redis.local
    # $db walks up to parent, finds sibling entry
    same_host: [call $= $host $db.host]
  ]
]
```

Here, `$db` inside `cache` walks up to the parent dict and finds the sibling `db` entry. `$host` resolves to `redis.local` (the local binding shadows any outer `host`).

The builtin scope (stdlib functions like `$+`, `$map`, `$eval`, etc.) is the root of the parent chain. Every expression's scope ultimately inherits from builtins.

#### Comparison with Other Languages

| Language | Within-block scoping | Sequential scoping | LLT equivalent |
|----------|---------------------|-------------------|----------------|
| Haskell | `where` / `let` (letrec, mutual recursion) | top-level defs (single letrec) | Dict entries |
| OCaml | `let rec ... and ...` (explicit letrec) | `let x = ... in let y = ...` (sequential) | Expr 1 then Expr 2 |
| Scheme | `letrec` (mutual visibility) | `let*` (sequential, each sees prior) | Both available |
| JavaScript | Block scope (`const`/`let`, no mutual ref) | Sequential statements | Different: JS has no letrec |
| Nix | Attribute set (`rec { }`, mutual ref) | `let ... in` (sequential) | Similar to LLT |
| Jsonnet | Object (self/super, late binding) | No sequential model | Similar within-block |

LLT is closest to **Nix**: `rec { }` attribute sets are letrec (mutual visibility), and `let x = ...; in` introduces sequential bindings. The key difference is that LLT uses the same `[...]` syntax for both -- a single dict is letrec, and sequential expressions in a document form a chain. There is no separate `let` keyword.

#### Between Documents: Total Isolation via `$$`

`---` separates independent documents. Documents have no shared scope — as if they were in separate files.

Data flows between documents via `$$`, a variable injected into each document's root scope containing the previous document's output. For the first document in a file, `$$` is `[]` (empty dict). `$$` is VarRef("$") -- no grammar special case needed since `$` is a valid identifier character under the denylist rules.

The name `$_` was considered but is used as the implicit lambda shorthand (see "No Auto-Curry" section).

```lisp
# Document 1 — $$ is []
[
  users: [
    [name: Alice  age: 30]
    [name: Bob    age: 25]
  ]
]
---
# Document 2 — $$ is Document 1's output (lazy)
[
  adults: [call $filter [fn [u] [call $>= $u.age 18]] $$.users]
]
---
# Document 3 — $$ is Document 2's output (lazy)
# Final expression is the program's output, serialized by the CLI
[call $eval $$]
```

The `---` boundary does **not** materialize the previous document. `$$` is a lazy dict — values are materialized only when accessed.

#### Pure Language, CLI Handles I/O

LLT is a pure data transformation language with no in-language side effects. The program evaluates to a value; the CLI serializes it:

```
llt eval file.llt              # evaluate, output result as JSON
llt eval -f yaml file.llt      # output as YAML (not yet implemented -- deferred)
llt eval --eval file.llt       # deep-force all thunks before serializing (surfaces errors before partial output)
llt eval -                     # read LLT source from stdin
cat data.json | llt eval file.llt  # stdin JSON parsed and injected as $$ for first document
```

This is the Jsonnet/Nix model: the language produces data, an external tool handles I/O. Unreferenced dict entries are never computed. There is no `$write`, `$read`, `$stdout`, `$stdin`, or channel system.

`$eval` is a runtime-supported function that recursively forces all thunks in its argument. It performs full materialization: the entire structure is forced into memory. The implementation caps recursion at depth 256 and returns an error if exceeded. On infinite or cyclic structures, `$eval` will hit the depth limit rather than diverging. Use `$take` to bound infinite sequences before passing them to `$eval`.

```lisp
# Without $eval: CLI serializes lazily (streaming, may partially output then hit an error)
[result: [call $map $$.data [fn [x] [call $+ $x 1]]]]

# With $eval: everything forced into memory first (errors caught before any output)
[result: [call $eval [call $map $$.data [fn [x] [call $+ $x 1]]]]]

# Safe on infinite sequences: $take bounds before $eval
[result: [call $eval [call $take 100 $$.sequence]]]
```

**Why pure?** In-language I/O in a lazy language creates a forcing problem: side-effecting expressions buried in lazy dict entries may never execute, and execution order becomes unpredictable. By making the language pure, lazy evaluation is semantically transparent — the result is the same regardless of evaluation order. The CLI is the only I/O boundary, and it forces exactly what it needs to serialize the output.

**Security:** External input (stdin, files) is parsed by the CLI and injected as structured data (`$$`). The language never evaluates untrusted input as code. `$from-json` is a pure function that converts a JSON string to a dict — safe on untrusted input.

#### Include Mechanism

`$include` evaluates a file and returns its dict. Two usage patterns:

**Namespaced** (like Python's `import module`):

```lisp
[
  utils: [call $include "lib/utils.llt"]
  result: [call $utils.double 21]
]
```

**Merged into scope** (like Python's `from module import *`):

Uses the sequential-expression scope chain. The included dict becomes a scope in the parent chain:

```lisp
[call $include "lib/utils.llt"]

# $double is visible via parent scope
[
  result: [call $double 21]
]
```

Note: the merged include becomes a *parent* scope, so the included file cannot reference names defined in the local dict that follows it. This matches the semantics of other languages' merge-style imports:

| Language | Merge import | Can imported code see local names? |
|----------|-------------|-----------------------------------|
| Python | `from utils import *` | No — `utils` can't see the importer's locals |
| Nix | `with pkgs; { ... }` | No — `pkgs` attrs are fixed at definition site |
| Haskell | `import Module` | No — module was compiled independently |
| JavaScript | `import * from './utils'` | No — module has its own scope |

If the included file needs to reference local bindings, use namespaced import instead and pass values explicitly:

```lisp
[
  utils: [call $include "lib/utils.llt"]
  result: [call $utils.make-config localhost 5432]
]
```

Duplicate names during merge are errors (consistent with the duplicate-keys-are-errors rule). Include cycle detection is required — even with lazy values, the scope structure must be known at include time.

### Document Separator Grammar

The `---` separator is recognized at the file level only. It must appear on its own (not as part of a bare word like `----` or `---foo`):

```pest
file          = { SOI ~ document ~ (doc_separator ~ document)* ~ EOI }
document      = { expression* }
expression    = { !doc_separator ~ value }
doc_separator = @{ "---" ~ !bare_word_char }
```

---

## Language vs Stdlib

Tracking what must be built into the language vs what can be implemented in the stdlib.

### Language Builtins (Special Forms)

These require special evaluation or parsing rules — they can't be expressed as regular functions. The parser recognizes them by checking the first entry of every `[]`:

- `call` — function application (exact arity required)
- `fn` — function definition (creates scope, binds params)
- `type` — type alias declaration

### Stdlib Functions

These leverage lazy evaluation and can be regular functions. Each function is classified by its **thunk behavior** — whether it preserves thunks, creates new ones, or materializes values:

- **Structural** — rearranges entries without inspecting values. Thunks pass through untouched.
- **Lazy-transforming** — applies a function to values but produces new thunks. No computation until the result is materialized.
- **Materializing** — must compute values to determine the result.
- **Selective** — materializes some arguments, leaves others as thunks (e.g., short-circuit evaluation).

**Control flow** (selective):

| Function | Materialization behavior |
|----------|------------------------|
| `if` | **Current:** Materializes condition and the chosen branch. **Phase 5b:** Will return the chosen branch as a thunk (other never materialized). |
| `cond` | Materializes conditions in order; returns first matching branch as thunk |
| `when`, `unless` | Materializes condition; returns body or `[]` |
| `and` | Materializes first; if false, returns false without materializing second |
| `or` | Materializes first; if true, returns true without materializing second |
| `not` | Materializes its argument |

**List operations** (integer keys only, always renumber to dense 0..n):

| Function | Materialization behavior |
|----------|------------------------|
| `first`, `rest` | Structural — returns thunks in new positions |
| `cons`, `conj`, `concat` | Structural — combines thunks into new structure |
| `reverse`, `reindex` | Structural — reorders/renumbers, values untouched |
| `sort`, `sort-by` | **Materializing** — must compare values to determine order. `$sort` uses lexicographic comparison for strings, numeric comparison for numbers. Sorting mixed types (e.g., strings and numbers in the same collection) is a type error caught at compile time. |

**Dict operations** (any key type, preserve keys):

| Function | Materialization behavior |
|----------|------------------------|
| `get`, `get-or`, `has?` | Structural — key lookup, returns thunk |
| `get-in` | **Materializing** — deep path access. Takes a dict and a list of keys, traverses nested dicts. Must evaluate each key lookup. |
| `set`, `remove` | Structural — add/remove entries |
| `merge` | **Current:** Eagerly clones both input dicts. **Phase 5b:** Will use lazy overlay (right dict's keys shadow left dict's keys, no deep copy until access). |
| `keys` | Structural — keys are always evaluated, not thunks |
| `values`, `entries` | Structural — returns thunks |
| `update` | Lazy-transforming — produces thunk `[call $f $old-value]` |

**Universal collections** (any collection, preserve keys, insertion order):

| Function | Materialization behavior |
|----------|------------------------|
| `nth`, `last`, `slice` | Structural — positional access, returns thunks |
| `take`, `drop` | Structural — positional subsequence, thunks preserved |
| `zip` | Structural — pairs entries, values stay thunks |
| `length`, `empty?` | Structural — counts entries, ignores values |
| `map`, `map-entries` | Lazy-transforming — on dicts, returns dict with PendingCall thunks; on seqs, returns lazy seq |
| `filter` | On dicts, returns Seq (must evaluate predicates); on seqs, returns lazy seq |
| `reduce`, `fold` | **Materializing** — accumulates, materializes each step |
| `find-deep` | **Materializing** — must traverse structure looking for keys |
| `flatten` | **Materializing** — must inspect values to check if they are lists |

**Arithmetic & comparison** (materializing — must evaluate operands):
- `+`, `-`, `*` (auto-promote: Int op Int → Int, mixed → Float)
- `/` (always returns Float), `quot`, `mod` (Int only, return Int; both are prelude functions)
- `=`, `<`, `>`, `<=`, `>=` (work on Int, Float, String, Bool; cross-type Int/Float comparison allowed). `$=` returns `false` for Dict, Function, and Builtin values -- there is no structural/deep equality. This will be revisited when typeclasses are added.
- `to-int`, `to-float`, `floor`, `ceil`, `round` (numeric conversions)

**Strings** (materializing — must evaluate arguments):
- `str` (exact concat), `words` (split by space, filter empties), `join` (with separator)
- `split`, `replace`
- `upper`, `lower`, `trim`

**Composition** (structural — builds function pipelines, no values materialized):
- `->` (threading)
- `compose`
- `apply` — call function with list spread as positional args

**Sequences** (lazy computation -- produce `Seq` values):
- `range`, `repeat`, `cycle`, `iterate`, `unfold` -- constructors (finite or infinite)
- `seq` -- low-level cons: `[call $seq $head $tail-thunk]`
- `collect` -- materializes a Seq into a dict with integer keys
- `head`, `tail` -- destructors
- `seq?` -- type check

**Utility:**

| Function | Materialization behavior |
|----------|------------------------|
| `identity` | Structural — returns its argument as-is |
| `type-of` | **Materializing** — must evaluate to determine type. Returns `"Function"` for both user-defined functions and Rust-native builtins (intentionally indistinguishable to user code). |
| `assert` | **Materializing** — must evaluate condition |
| `error` | Structural — constructs error value, not materialized until propagated |
| `try`, `try-or` | **Materializing** — materializes body, catches exceptions. `$try` returns `[ok: value]` on success or `[err: message]` on failure (tagged dict, not a special type). |

**Materialization** (runtime-supported):
- `eval` — recursively forces all thunks (runtime-supported, may diverge on infinite structures)
- `from-json` — parses JSON string into LLT dict (pure function, safe on untrusted input)

**Key implications for lazy evaluation:**

```lisp
# $map on dict is lazy — returns dict with PendingCall thunks
big-result: [call $map [fn [x] [call $expensive $x]] $big-dict]
$big-result.widget          # Only this one element gets computed

# $filter on dict returns a Seq (must evaluate predicates to decide inclusion)
# Other fields on kept users remain thunks until accessed
expensive: [call $collect [call $filter [fn [x] [call $> $x.price 100]] $products]]

# $sort must materialize everything — can't sort without comparing
sorted: [call $sort $big-list]  # All values materialized immediately

# Infinite sequences — lazy all the way
naturals: [call $range 0]   # O(1), nothing computed
squares: [call $map [fn [n] [call $* $n $n]] $naturals]  # still O(1)
first-ten: [call $collect [call $take 10 $squares]]
# -> [0 1 4 9 16 25 36 49 64 81]
```

### Rust-Native vs LLT-Implemented Boundary

**Principle:** Only implement in Rust what cannot be expressed in LLT itself. Everything else is LLT code loaded from a prelude file at startup.

**Rust-native builtins (28 total):**

| Group | Functions | Rationale |
|-------|-----------|-----------|
| Arithmetic | `+`, `-`, `*`, `/` | Operate on host numeric types (i64, f64); no LLT primitive can perform arithmetic. |
| Comparison | `=`, `<` | Compare host values; cross-type Int/Float comparison requires host-level coercion. `>`, `<=`, `>=` are derived from `<` and `not`. |
| Control | `if` | Requires selective materialization (only materialize the chosen branch). `not` is derived: `[fn [x] [call $if $x false true]]`. |
| Dict primitives | `keys`, `length`, `merge`, `append` | Operate on the IndexMap directly: `keys` extracts the key set, `length` reads `IndexMap::len()`, `merge` right-biased combines two IndexMaps, `append` inserts a value at the next integer key. |
| Strings | `str`, `split`, `replace`, `upper`, `lower`, `trim` | Strings are opaque values; all operations that inspect or transform string content require Rust. `join` and `words` are derived from `str`/`split` + recursion. |
| Numeric | `floor`, `round` | `floor` truncates toward negative infinity (Rust `f64::floor`). `round` rounds half-away-from-zero (Rust `f64::round`). `ceil` and `trunc` are derived from `floor` and comparison. |
| Parsing | `to-int`, `to-float` | String-to-number parsing only (e.g., `"42"` to `42`). Numeric conversion (float-to-int) uses `floor`/`round`/`trunc`; int-to-float uses arithmetic promotion (`[call $+ $x 0.0]`). |
| Evaluation control | `eval`, `error`, `try`, `apply` | `eval` deep-forces thunks (evaluator access); `error` constructs EvalError; `try` catches materialization errors; `apply` spreads a dict as positional args. |
| Type introspection | `type-of` | Inspects the Value enum variant; no LLT expression can determine a value's type. |
| I/O | `from-json`, `include` | `from-json` parses a JSON string into an LLT dict; requires a JSON parser (serde_json). `include` evaluates an LLT file and returns its result; requires filesystem access, cycle detection, and path resolution. |

**Derived functions (moved from Rust to LLT):**

| Function | Derivation | Why not Rust |
|----------|-----------|--------------|
| `not` | `[fn [x] [call $if $x false true]]` | `if` already handles Bool dispatch |
| `>` | `[fn [a b] [call $< $b $a]]` | Argument swap |
| `<=` | `[fn [a b] [call $not [call $< $b $a]]]` | Negated `>` |
| `>=` | `[fn [a b] [call $not [call $< $a $b]]]` | Negated `<` |
| `quot` | `[fn [a b] [call $trunc [call $/ $a $b]]]` | Truncation toward zero (Clojure semantics) |
| `mod` | `[fn [a b] [call $- $a [call $* [call $quot $a $b] $b]]]` | Algebraic identity: `a - (a quot b) * b` |
| `ceil` | `[fn [x] [call $- 0 [call $floor [call $- 0 $x]]]]` | `ceil(x) = -floor(-x)` |
| `trunc` | `[fn [x] [call $if [call $>= $x 0] [call $floor $x] [call $ceil $x]]]` | Conditional floor/ceil |
| `join` | Recursive accumulation with `str` | String concatenation via `str` + recursion |
| `words` | `[call $filter [fn [w] [call $not [call $= $w ""]]] [call $split " " $s]]` | `split` + `filter` |

Note: `and` and `or` are also LLT-derived (`[fn [a b] [call $if $a $b false]]` works via lazy args, giving short-circuit semantics for free). Similarly, `get` is just `[fn [xs k] $xs[$k]]` using bracket access.

**LLT-implemented stdlib:**

Everything else is implemented in LLT using the Rust builtins above plus language features (bracket access, dict literals, `fn`, `call`, recursion via letrec). Key implementation patterns:

- **Derived primitives**: `not` from `if`, comparison operators from `<`, `mod` from arithmetic, `ceil`/`trunc` from `floor`, `join`/`words` from `str`/`split`.
- **Short-circuit logic** via lazy args: `and` = `[fn [a b] [call $if $a $b false]]`.
- **Dict iteration** via `keys` + bracket access + recursion: `map`, `filter`, `reduce` all follow the pattern of getting `keys`, iterating with a recursive helper, building results with `merge`.
- **Dict utilities** as wrappers: `get` = `[fn [xs k] $xs[$k]]`, `empty?` = `[fn [xs] [call $= [call $length $xs] 0]]`, `has?` wraps `try` around bracket access.
- **Control flow** from `if`: `cond` = nested `if`, `when`/`unless` = single-arm `if`.
- **Composition** is pure LLT: `identity` = `[fn [x] $x]`, `compose` = `[fn [f g] [fn [x] [call $f [call $g $x]]]]`.
- **List operations** use `keys` + `length` + `merge` + recursion to build new integer-keyed dicts.

**Loading mechanism:**

The LLT stdlib lives in `stdlib/prelude.llt`, bundled at compile time via `include_str!`. At startup:

1. Create root environment with Rust-native builtins
2. Parse and evaluate `prelude.llt` with root environment as parent
3. User code's environment inherits from the stdlib environment

This ensures Rust builtins are available to LLT stdlib code (e.g., `and` references `$if`), and user code sees both layers:

```
Rust builtins ($+, $-, $<, $=, $if, $keys, $merge, $str, $floor, ...)
  └── LLT stdlib ($not, $>, $mod, $ceil, $and, $map, $filter, $compose, ...)
        └── User code
```

### Stdlib Function Reference (62 functions)

All functions below are implemented in LLT itself in `stdlib/prelude.llt`. They are loaded at startup via `create_stdlib_env()` and available to all user code. Private implementation details (functions suffixed with `-impl`) are omitted.

**Utility Functions:**

Functions primarily used internally by other stdlib functions, but also available to user code.

| Function | Signature | Description |
|----------|-----------|-------------|
| `make-entry` | `[fn [k v] ...]` | Construct a single-entry dict from a computed key and value |

**Identity:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `identity` | `[fn [x] $x]` | Returns its argument unchanged |

**Logic:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `not` | `[fn [x] ...]` | Boolean negation |
| `and` | `[fn [a b] ...]` | Short-circuit AND: returns `$b` if `$a` is true, else `false` |
| `or` | `[fn [a b] ...]` | Short-circuit OR: returns `true` if `$a` is true, else `$b` |

**Comparison (derived from `<` and `=`):**

| Function | Signature | Description |
|----------|-----------|-------------|
| `>` | `[fn [a b] ...]` | Greater than |
| `<=` | `[fn [a b] ...]` | Less than or equal |
| `>=` | `[fn [a b] ...]` | Greater than or equal |

**Arithmetic (derived from `+`, `-`, `*`, `/`):**

| Function | Signature | Description |
|----------|-----------|-------------|
| `quot` | `[fn [a b] ...]` | Integer quotient, truncates toward zero (Clojure semantics) |
| `mod` | `[fn [a b] ...]` | Remainder: `a - (a quot b) * b` |

**Numeric Conversion (derived from `floor`):**

| Function | Signature | Description |
|----------|-----------|-------------|
| `ceil` | `[fn [x] ...]` | Ceiling: smallest integer >= x. Derived as `-floor(-x)` |
| `trunc` | `[fn [x] ...]` | Truncate toward zero: `floor` for positive, `ceil` for negative |

**String (derived from `str`, `split`, `filter`):**

| Function | Signature | Description |
|----------|-----------|-------------|
| `join` | `[fn [sep xs] ...]` | Join a list of strings with a separator |
| `words` | `[fn [s] ...]` | Split a string by spaces, filtering empty strings |

**Control Flow:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `when` | `[fn [pred body] ...]` | Returns `$body` if `$pred` is true, else `[]` |
| `unless` | `[fn [pred body] ...]` | Returns `$body` if `$pred` is false, else `[]` |
| `cond` | `[fn [pairs] ...]` | Multi-branch conditional: takes a list of `[condition result]` pairs |

**Dict Utilities:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `get` | `[fn [xs k] ...]` | Get value by key (bracket access wrapper) |
| `has?` | `[fn [xs k] ...]` | Check if a key exists (uses `$try` around access) |
| `get-or` | `[fn [xs k default] ...]` | Get value by key with fallback default |
| `get-in` | `[fn [xs path] ...]` | Traverse nested dicts by a list of keys |
| `empty?` | `[fn [xs] ...]` | Check if a collection has zero entries |
| `set` | `[fn [xs k v] ...]` | Return new dict with key added/updated |
| `remove` | `[fn [xs k] ...]` | Return new dict with key removed |
| `update` | `[fn [xs k f] ...]` | Apply function `$f` to the value at key `$k` |
| `values` | `[fn [xs] ...]` | Get all values as an integer-indexed list |
| `entries` | `[fn [xs] ...]` | Get all entries as a list of `[key: k value: v]` dicts |

**List Operations (integer keys, dense 0..n output):**

| Function | Signature | Description |
|----------|-----------|-------------|
| `first` | `[fn [xs] ...]` | Get the first element (key 0) |
| `nth` | `[fn [xs n] ...]` | Get element by insertion-order position (supports negative indices) |
| `last` | `[fn [xs] ...]` | Get the last element by insertion-order position |
| `rest` | `[fn [xs] ...]` | All elements except the first, reindexed from 0 |
| `cons` | `[fn [x xs] ...]` | Prepend an element, reindexing from 0 |
| `conj` | `[fn [xs x] ...]` | Append an element (delegates to `$append`) |
| `concat` | `[fn [xs ys] ...]` | Concatenate two lists, reindexing the second |
| `reverse` | `[fn [xs] ...]` | Reverse a list |
| `reindex` | `[fn [xs] ...]` | Rebuild with dense 0..n integer keys |

**Sorting:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `sort` | `[fn [xs] ...]` | Sort using natural ordering (mergesort) |
| `sort-by` | `[fn [cmp xs] ...]` | Sort using a custom comparator function |

**Universal Collection Operations (preserve keys):**

| Function | Signature | Description |
|----------|-----------|-------------|
| `map` | `[fn [f xs] ...]` | Apply function to every value, preserving keys |
| `map-entries` | `[fn [f xs] ...]` | Apply function to every `[key value]` pair |
| `filter` | `[fn [pred xs] ...]` | Keep entries where predicate returns true |
| `reduce` | `[fn [f init xs] ...]` | Left fold over a collection |
| `fold` | `[fn [f init xs] ...]` | Alias for `reduce` |
| `slice` | `[fn [xs start end] ...]` | Positional slice (start inclusive, end exclusive) |
| `take` | `[fn [n xs] ...]` | Take the first n entries, preserving keys |
| `drop` | `[fn [n xs] ...]` | Skip the first n entries, preserving keys |
| `zip` | `[fn [xs ys] ...]` | Pair entries from two collections by position |
| `flatten` | `[fn [xs] ...]` | Flatten nested lists one level deep |
| `find-deep` | `[fn [xs target] ...]` | Recursively search for a key in nested dicts |

**Composition:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `compose` | `[fn [f g] ...]` | Compose two functions: `(compose f g)(x) = f(g(x))` |
| `->` | `[fn [x ...stages] ...]` | Thread a value through a series of functions |

**Error Handling:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `try-or` | `[fn [f default] ...]` | Call a function; return default if it errors |

**Sequences:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `range` | `[fn [start ...end] ...]` | Seq of integers from start; finite if end given, infinite otherwise |
| `repeat` | `[fn [val ...n] ...]` | Seq of copies of val; finite if n given, infinite otherwise |
| `cycle` | `[fn [xs ...n] ...]` | Seq repeating xs; finite if n given, infinite otherwise |
| `iterate` | `[fn [f x] ...]` | Infinite seq: x, f(x), f(f(x)), ... |
| `unfold` | `[fn [step seed] ...]` | Seq from step function; step returns `[value state]` or `[]` to stop |
| `seq` | `[fn [head tail] ...]` | Low-level seq constructor (cons cell) |
| `collect` | `[fn [s] ...]` | Materialize seq into dict with integer keys 0..n |
| `head` | `[fn [s] ...]` | First element of seq |
| `tail` | `[fn [s] ...]` | Rest of seq (seq, not materialized) |
| `seq?` | `[fn [x] ...]` | True if x is a Seq |

**Assertions:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `assert` | `[fn [cond msg] ...]` | Assert condition; error with message if false |

### Sequences and Lazy Computation

**Sequences are lazy computations, not data.** Dicts are data (finite, random-access, known keys). Sequences are suspended computations that produce elements on demand (possibly infinite, sequential access, unknown structure).

This distinction preserves the "everything is a dict" invariant for data while enabling lazy, composable pipelines for computation.

**Runtime representation:**

A sequence is a cons cell: a head value and a tail that is itself a sequence (or empty dict `[]` for end-of-sequence).

```
Value::Seq(head: Rc<Thunk>, tail: Rc<Thunk>)
```

The tail thunk evaluates to either another `Seq` or `[]` (done). Since thunks are memoized, traversing the same sequence twice reuses cached results -- unlike Python generators, which are single-pass.

**`$collect` is the boundary between computation and data:**

```lisp
# Computation (lazy, possibly infinite)
evens: [call $filter [fn [n] [call $= 0 [call $mod $n 2]]] [call $range 0]]

# Data (materialized, finite, dict with integer keys)
first-ten: [call $collect [call $take 10 $evens]]
# -> [0 2 4 6 8 10 12 14 16 18]
```

`$collect` runs the computation and pours results into a dict with integer keys 0..n. Calling `$collect` on an infinite sequence without `$take` is an error (hits depth/memory limit). This is explicit by design -- no accidental infinite materialization.

**Sequence constructors:**

| Function | Finite | Infinite | Description |
|----------|--------|----------|-------------|
| `range` | `[call $range 0 10]` | `[call $range 0]` | Integers from start; optional end (exclusive) |
| `repeat` | `[call $repeat 5 x]` | `[call $repeat x]` | n copies of a value; or infinite |
| `cycle` | `[call $cycle 3 xs]` | `[call $cycle xs]` | Repeat a list n times; or infinite |
| `seq` | -- | -- | Low-level: `[call $seq $head $tail-thunk]` |
| `iterate` | -- | `[call $iterate $f $x]` | `x, f(x), f(f(x)), ...` |
| `unfold` | varies | varies | `[call $unfold $step $seed]`; step returns `[value state]` or `[]` |

**Sequence operations (lazy -- return sequences):**

| Function | Description |
|----------|-------------|
| `take` | First n elements |
| `drop` | Skip first n elements |
| `filter` | Elements matching predicate |
| `map` | Transform each element (on seq input; on dict input, returns lazy dict) |
| `concat-seq` | Concatenate two sequences |
| `zip-seq` | Pair elements from two sequences |

**Sequence destructors (materializing):**

| Function | Description |
|----------|-------------|
| `collect` | Seq to dict with integer keys 0..n |
| `head` | First element (materializes head thunk) |
| `tail` | Rest of sequence (returns seq, does not materialize) |
| `reduce` | Accumulate over sequence elements |
| `seq?` | Type check: is this a Seq? |

**Dual dispatch for `$map` and `$filter`:**

`$map` and `$filter` accept both dicts and sequences, with behavior determined by input type:

| Input | `$map` result | `$filter` result |
|-------|--------------|-----------------|
| Dict | Dict (lazy values via PendingCall thunks) | Seq (must evaluate predicates) |
| Seq | Seq (lazy) | Seq (lazy) |

`$map` on a dict is the key insight: it returns a dict with the **same keys** but each value wrapped in a `PendingCall` thunk. No computation happens until a specific value is accessed. This makes `[call $map $f $big-dict]` O(n) to construct and O(1) per element access, compared to the current O(n^2) eager implementation.

`$filter` on a dict must return a Seq because the output keys are unknown without evaluating predicates. Use `$collect` to get a dict back.

```lisp
# $map on dict: same keys, lazy values (no computation yet)
prices-usd: [call $map [fn [p] [call $* $p 1.1]] $prices-eur]
$prices-usd.widget    # only this one price is computed

# $filter on dict: returns seq (must evaluate predicates to decide inclusion)
expensive: [call $collect [call $filter [fn [p] [call $> $p 100]] $prices-eur]]

# $map on seq: returns seq (lazy)
doubled: [call $map [fn [n] [call $* $n 2]] [call $range 0]]
# nothing computed until $take/$collect
```

**`PendingCall` thunk state:**

To make dict-returning operations lazy, the thunk model gains a new state:

```
PendingCall(function: Rc<Thunk>, args: Vec<Rc<Thunk>>)
```

`PendingCall` represents "apply this function to these arguments when forced." It enables lazy function application at runtime without constructing AST nodes. When a `PendingCall` thunk is materialized, it calls the function and memoizes the result, just like `PendingBuiltin` does for builtin calls.

**`PendingBuiltin` preserves laziness:** When the evaluator encounters `[call $builtin ...]`, it does not immediately execute the builtin. Instead, it wraps the builtin name and unevaluated argument thunks in a `PendingBuiltin` state. The builtin executes only when the result is materialized (accessed). This deferred execution is critical for preserving lazy semantics — builtins like `$if` can selectively materialize arguments, and operations like `$map` can return lazy structures without forcing computation.

This completes the laziness picture:

| Thunk state | Represents | Created by |
|-------------|-----------|-----------|
| `Unevaluated` | AST expression + environment | Parser/eval (dict values, fn bodies) |
| `PendingBuiltin` | Deferred builtin call | `[call $builtin ...]` |
| `PendingCall` | Deferred function application | `$map`, `$update`, lazy combinators |
| `InProgress` | Cycle detection sentinel | Materialization |
| `Materialized` | Computed value | After first force |

**Impact on existing operations:**

With `PendingCall` and `Seq`, several operations become lazier:

| Operation | Before | After |
|-----------|--------|-------|
| `$map f dict` | Eager, O(n^2) | Lazy dict with PendingCall values, O(n) construct / O(1) per access |
| `$filter pred dict` | Eager, O(n^2) | Returns Seq, O(1) construct / O(n) to fully consume |
| `$range start end` | Eager dict, O(n^2) | Seq, O(1) to construct |
| `$range start` | Not possible | Infinite Seq, O(1) |
| `$merge a b` | Eager clone | Lazy overlay (b's keys shadow a's, no deep copy) |
| `$if cond t f` | Materializes chosen branch | Returns chosen branch as thunk |
| `$update dict k f` | Eager | PendingCall on the updated value |

**BuiltinFn signature change:**

To support builtins that return lazy results, `BuiltinFn` changes from returning `Value` to returning `Rc<Thunk>`:

```
// Before
type BuiltinFn = fn(args, named, depth, call_span) -> Result<Value, Box<EvalError>>;

// After
type BuiltinFn = fn(args, named, depth, call_span) -> Result<Rc<Thunk>, Box<EvalError>>;
```

Builtins that currently return materialized values wrap them in `Thunk::new_materialized()`. Builtins like `$map` and `$if` can now return thunks directly. This removes the forced materialization boundary that currently prevents builtins from participating in lazy evaluation.

---

## Open Questions / TODO

Design questions that still need to be resolved. All other design questions have been resolved and appear in the Confirmed Decisions section above.

### Structural Contracts

- [ ] **Shape/contract system** — Predicate-based validation separate from the type system. Allows runtime constraints beyond what types express (e.g., "port must be 1-65535").
- [ ] **OpenAPI integration** — Load external schemas as contracts for validation.
- [ ] **Lazy vs eager validation** — Validate on materialization vs explicit `[call $validate! $schema $data]`?

---

## Future Features

Evaluate these later:

- **Parameterized type aliases** — `Mapper: [type [a b] [Fn@b [a]]]`. Deferred until variable name collision becomes a real problem. Textual expansion is sufficient for now.
- **`let` binding form** — Use dict entries for all bindings for now. Add `let` for local bindings if needed later.
- **Pattern matching** — Not yet designed.
- **Quasiquoting** — Not yet specified.
- **Custom call aliases** — Users can define their own; no built-in alias for `call`.
- **Gradual typing** — Decided mandatory types instead, but could revisit for specific use cases.
- **`list?` vs `dict?` predicates** — Since lists are dicts, need to decide if/how to distinguish at runtime (probably shouldn't).
- **String interpolation** — `"Hello $name"` in double-quoted strings. Deferred because `$str`/`$words` cover the need. `$` sigils make future interpolation natural and non-breaking to add.
- **Float dict keys** — Floats as dict keys have precision issues (`0.1 + 0.2 ≠ 0.3`) and NaN is incomparable. Integer and string keys cover all current needs. Revisit if a use case arises.
- **Width-specific numeric types** — `Int32`, `Int64`, `Int128`, `UInt64`, `Float32`, `Decimal`, etc. These are range constraints on `Int`/`Float`, implementable via the contracts system once it exists. `Decimal` would need a new Value variant if precise decimal arithmetic is required.
- **Typeclasses** — Ad-hoc polymorphism for extensible numeric operations, generic serialization, custom equality/ordering. Not needed while the set of built-in types is small and the promotion table is hardcoded. Revisit if user-defined types need to participate in built-in protocols.

---

## Common Patterns

### Shared Base Config

```lisp
[
    base: [timeout: 30  retries: 3]
    dev:  [call $merge $base [env: dev]]
    prod: [call $merge $base [env: prod  timeout: 60]]
]
```

### List Transformation

```lisp
[
    users: [...]

    # Filtering and projection
    admin-users: [call $filter [fn [u] $u.is-admin] $users]
    user-names:  [call $map [fn [u] $u.name] $users]

    # Complex predicates
    senior-admins: [call $filter [fn [u] [call $and $u.is-admin [call $> $u.age 40]]] $users]
    user-summaries: [call $map [fn [u] [n: $u.name  a: $u.age]] $users]

    # List operations (renumber to clean list)
    reversed: [call $reverse $user-names]
    sorted: [call $sort $user-names]
    without-first: [call $rest $users]

    # Filter + reindex for clean dense list
    active: [call $-> $users
        [call $filter $_.active $_]
        $reindex]
]
```

### Conditional Logic

```lisp
[
    mode: production
    config: [call $if [call $= $mode production]
        [timeout: 60  logging: error]
        [timeout: 10  logging: debug]]

    # Conditional — returns body or [] (empty dict)
    debug-info: [call $when [call $= $mode dev] [trace: on  verbose: true]]
]
```

### Template Function

```lisp
[
    make-service: [fn@[name: String  port: Number  health: String] [name@String  port@Number]
        [name: $name  port: $port  health: "/health"]]

    web: [call $make-service web 8080]
    api: [call $make-service api 3000]
]
```

### Pipeline (Using Stdlib Threading)

```lisp
[call $-> $raw-data
    [call $filter $active? $_]
    [call $map $extract-name $_]
    [call $sort-by $last-name $_]]
```

---

## Architecture

### Components

```
┌─────────────┐
│   Source     │  .llt file (documents separated by ---)
└──────┬──────┘
       │
       ▼
┌─────────────┐
│   Parser     │  Text → AST (File > Document > Expr)
└──────┬──────┘
       │
       ▼
┌─────────────┐
│  Type Check  │  Infer & verify types
└──────┬──────┘
       │
       ▼
┌─────────────┐
│  Evaluator   │  Per-document: scope chains, $$ pipeline, lazy
└──────┬──────┘
       │
       ▼
┌─────────────┐
│     CLI      │  Input parsing, $eval, output serialization
└─────────────┘
```

> **Note:** The type checker (Phase 2a/2b) runs after parsing but type errors are advisory -- evaluation proceeds regardless of type errors. This matches the design philosophy that types aid development without blocking execution.

### Implementation Roadmap

See [TODO.md](TODO.md) for the full checklist with current status.

### Sketch: Value Enum

> **Note:** This sketch captures the original design intent. The authoritative implementation is in `src/value.rs`, `src/error.rs`, and `src/ast.rs`, which refine these types (e.g., `IndexMap` for insertion order, `Vec<Param>` for full parameter metadata, `Span` for source locations).

```rust
enum Value {
    Int(i64),
    Float(f64),
    String(String),
    Bool(bool),
    Dict(LinkedHashMap<Key, Rc<Thunk>>),
    Seq(Rc<Thunk>, Rc<Thunk>),    // head, tail (tail evaluates to Seq or [] for end)
    Function {
        params: Vec<String>,
        body: AstNode,
        env: Environment,
    },
    Builtin(fn(BuiltinArgs) -> Result<Rc<Thunk>, Error>),
    // BuiltinArgs { positional: Vec<Rc<Thunk>>, named: IndexMap<String, Rc<Thunk>> }
}

struct Thunk {
    expr: AstNode,
    env: Environment,
    state: RefCell<ThunkState>,
    source: SourceLocation,       // definition-site location
}

enum ThunkState {
    Unevaluated,
    PendingBuiltin(name, args),   // deferred builtin call
    PendingCall(func, args),      // deferred function application (lazy $map, $update, etc.)
    InProgress,                   // cycle detection — hitting this during materialization means circular dep
    Materialized(Value),
}

struct SourceLocation {
    file: String,
    line: usize,
    column: usize,
}

enum Key {
    Int(i64),           // signed — negative integer keys are valid
    String(String),
}

struct Environment {
    bindings: HashMap<String, Rc<Thunk>>,
    parent: Option<Rc<RefCell<Environment>>>,   // mutable — letrec needs self-referential bindings
}
```

### Compiler Notes: Strictness Analysis

**Materialization behavior is inferred by the compiler, not annotated in the type system.** The stdlib listing documents which functions are structural, lazy-transforming, materializing, or selective — but this is documentation for humans, not a language feature.

**Why not a type-level annotation:**
- Redundant — the annotation would restate what the code already does
- Fragile — refactoring internals could invalidate the annotation
- Over-simplified — real materialization behavior is conditional and nuanced (e.g., `$filter` materializes predicates but not passed-through values). No annotation captures "materialized only when the collection is non-empty."
- Burden — one more thing the programmer writes and maintains

**Compiler responsibilities:**
- **Demand analysis** — examine function bodies to determine which arguments are always materialized, sometimes materialized, or never materialized. Analogous to GHC's demand analyzer.
- **Builtin metadata** — builtins are implemented in Rust, so the compiler can't analyze their bodies. Materialization behavior must be manually declared as metadata on the Rust side.
- **Dead thunk detection** — warn when an expression is never materialized (dead code under lazy eval).

**Tooling integration:**
- LSP hover: show which arguments will be materialized at a call site
- LSP inlay hints: `[materialized]` / `[lazy]` next to arguments
- Auto-generated docs: annotate stdlib reference with materialization behavior

**Deferred: explicit materialize annotation (`!`).** An expression-level `[! expr]` to materialize eagerly at binding time. Useful for performance tuning but not needed until lazy eval is implemented and profiled.

---

## Syntax Reference

```lisp
# Document structure
# A file contains documents separated by ---
# Each document contains one or more expressions
# Sequential expressions form a scope chain

[x: 10]                         # Expression 1
[y: [call $+ $x 1]]            # Expression 2 (sees x from parent scope)
---                             # Document separator (total isolation)
[z: $$.x]                      # New document ($$ is previous doc's output)

# Data
[key: value]                    # Dict (key and value are strings)
[a b c]                         # List (equivalent to [0: a  1: b  2: c])
[]                              # Empty dict/list
"hello world"                   # Quoted string (needed for spaces/special chars)
hello                           # Bare string
42                              # Int
3.14                            # Float
true false                      # Bool

# Mixed keyed/unkeyed
[call $f $x timeout: 60]        # Positional + named entries in one []
[a b key: val c]                # ERROR — positional after named

# References
$x                              # Variable reference
[$key: $value]                  # Computed key and value

# Key-based access (brackets and dot — desugars to $get)
$person.name                    # → [call $get $person name]
$config.database.host           # → chained $get
$data[5]                        # → [call $get $data 5]  key 5
$data[-1]                       # → [call $get $data -1] key -1, NOT last
$dict[$key]                     # → [call $get $dict $key]
$data[2..5]                     # → key-range slice: keys in [2, 5)
$config.services[0].host        # → mixed chaining

# Position-based access (functions, not syntax)
[call $nth $data 0]       # first entry by position
[call $nth $data -1]      # last entry (negative = from end)
[call $last $data]              # last entry (alias)
[call $slice $data 2 5]         # entries at positions 2, 3, 4

# Function application (exact arity required)
[call $f $arg1 $arg2]           # Positional args
[call $f $arg1 opt: $val]       # Named args (bare key-value)

# Implicit lambda ($_ shorthand)
[call $+ $_ 1]                  # → [fn [_] [call $+ $_ 1]]
[call $> $_.age 30]             # → [fn [_] [call $> $_.age 30]]

# Apply (spread list into function args)
[call $apply $f $arg-list]      # Spreads list entries as positional args

# Function definition
[fn@Number [x@Number  y@Number]
  [call $+ $x $y]]

# Named function (just a dict entry)
add: [fn@Number [x@Number  y@Number]
  [call $+ $x $y]]

# Named parameters (default: makes them named)
fetch: [fn@String [url@String  timeout@[type: Number  default: 30]]
  ...]

# Variadic parameters
apply-all: [fn [f ...args] [call $map $f $args]]

# Type alias
Name: [type TypeExpression]

# @ property annotations
param@Type                      # Shorthand: param@[type: Type]
param@[type: T  default: val]   # Full form with properties
fn@Type                         # Return type (shorthand)
fn@[type: T  doc: "..."]        # Return type with properties

# @ type assertions (on expressions)
[@Number $expr]                 # Assert type, throw on mismatch
[@[type: Number  default: 0] $expr]  # Safe cast with fallback

# Type expressions
[key: Type ...]                 # Open record type
[key: Type]                     # Closed record type
[Type]                          # List type
[Fn@b [a]]                     # Function type (mirrors fn definition)
Any                             # Dynamic escape hatch

# Materialization (explicit, runtime-supported)
[call $eval $$]                 # Recursively force all thunks into memory

# Include
utils: [call $include "lib/utils.llt"]   # Namespaced
[call $include "lib/utils.llt"]          # Merged into scope (as top-level expression)

# Conditionals (stdlib functions)
[call $if $cond $then $else]    # Returns $then or $else
[call $when $cond $body]        # Returns $body or [] (expression-safe)
[call $unless $cond $body]      # Returns $body or [] (expression-safe)

# Pipelines (using $_ shorthand for multi-arg functions)
[call $-> $data
    [call $filter [call $> $_.age 30] $_]  # two $_ levels: inner = element, outer = collection
    [call $map $_.name $_]                 # inner $_.name = element transform, outer $_ = collection
    $sort]                                 # Already 1-arg, no $_ needed

# Comments
# This is a comment
[x: 5]  # Inline comment
```

---

## Comparison

See the comparison table below for how LLT relates to JSONnet, Dhall, Nix, CUE, and jq.

| Need | Use |
|------|-----|
| Universal compatibility | JSON |
| DevOps convention | YAML |
| Large-scale Kubernetes | JSONnet |
| Type-safe configs | Dhall |
| Package management | Nix |
| Schema validation | CUE |
| Shell JSON transforms | jq |
| **Unified data + transformation** | **LLT** |

### Data Selection: jq / JSONPath / JMESPath / LLT

| Operation | jq | JSONPath | JMESPath | LLT |
|-----------|-----|---------|----------|-----|
| Field access | `.name` | `$.name` | `name` | `$data.name` |
| Nested access | `.a.b.c` | `$.a.b.c` | `a.b.c` | `$data.a.b.c` |
| Deep path (dynamic) | `getpath(p)` | N/A | N/A | `[call $get-in $data $path]` |
| Computed key | `.["k"]` | `$['k']` | N/A | `$data[$key]` |
| Key index | `.["k"]` | `$[0]`, `$[1]` | `[0]`, `[-1]` | `$data[0]`, `$data[-1]` (key-based) |
| Positional index | `.[0]`, `.[-1]` | N/A | N/A | `[call $nth $data 0]`, `[call $nth $data -1]` |
| Key-range slice | N/A | N/A | N/A | `$data[2..5]` (keys in range) |
| Positional slice | `.[2:5]` | `$[2:5]` | `[2:5]` | `[call $slice $data 2 5]` |
| First/last | `.[0]`, `.[-1]` | `$[0]` | `[0]`, `[-1]` | `$data[0]` (key 0), `[call $last $data]` |
| Flatten | `flatten` | N/A | `[]` | `[call $flatten $list]` |
| All values | `.[]` | `$.*` | `*` | `[call $values $data]` |
| Filter (simple) | `select(.age > 30)` | `[?(@.age>30)]` | `` [?age>`30`] `` | `[call $filter [fn [u] [call $> $u.age 30]] $data]` |
| Filter (complex) | `select(.a and .b)` | `$[?@.a && @.b]` | `[?a && b]` | `[call $filter [fn [u] [call $and $u.a $u.b]] $data]` |
| Projection | `.items[].name` | `$.items[*].name` | `items[*].name` | `[call $map [fn [x] $x.name] $items]` |
| Reshape | `{n: .name}` | N/A | `{n: name}` | `[call $map [fn [x] [n: $x.name  a: $x.age]]]` |
| Multi-select | `[.name, .age]` | N/A | `[name, age]` | `[$data.name  $data.age]` |
| Pipe/chain | `\|` | implicit | `\|` | `[call $-> ...]` |
| Optional access | `.foo?` | N/A | N/A | `[call $get-or $data foo default]` |
| Existence check | `has("key")` | N/A | N/A | `[call $has? $data key]` |
| Recursive descent | `..` | `$..name` | N/A | `[call $find-deep $data name]` |

## Resources

- [Crafting Interpreters](https://craftinginterpreters.com/) — evaluator implementation
- [Write You a Haskell](http://dev.stephendiehl.com/fun/) — lazy evaluation
- [pest.rs](https://pest.rs/) — PEG parser (used for `src/grammar.pest`)
