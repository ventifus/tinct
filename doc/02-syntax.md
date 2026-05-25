# Syntax

This document describes the tinct language syntax: its design rationale, formal grammar, tokenization rules, and quick reference. For evaluation semantics (how these constructs execute), see [Evaluation](08-evaluation.md). For the complete language documentation see [doc/index.md](index.md).

---

## 1. Quick Look

Here's what tinct code looks like:

```tinct
# Document structure
# A file contains documents separated by ---
# Each document contains one or more expressions
# Sequential expressions form a scope chain

[x: 10]                         # Expression 1
[y: [+ x 1]]                   # Expression 2 (sees x from parent scope)
---                             # Document separator (total isolation)
[z: %.x]                       # New document (% is previous doc's output)

# Named pipeline sections
--- %defaults
[host: "localhost"  port: 8080]

--- %overrides
[host: "prod.example.com"  tls: true]

---
[merge %defaults %overrides]   # access both named sections

# Data
[key: "value"]                  # Dict (keys are bare, values are quoted strings)
[$a $b $c]                      # Data sequence of references (escaped-ref head)
[]                              # Empty dict/list
"hello world"                   # Quoted string (needed for spaces/special chars)
42                              # Int
3.14                            # Float
true false                      # Bool

# Mixed keyed/unkeyed
[f x timeout: 60]               # Implied call with named argument
[a "b" key: "val" c]           # Positional and named freely interleaved

# References (bare words)
x                               # Variable reference
[$key: value]                   # Computed key ($-prefix) and bare reference value

# Dot access
person.name                     # dot access: Key::String("name") lookup
config.database.host            # chained dot access

# Position-based access (functions, not syntax)
[nth data 0]                    # first entry by position
[nth data -1]                   # last entry (negative = from end)
[last data]                     # last entry (alias)
[slice data 2 5]                # entries at positions 2, 3, 4

# Function application — implied call (preferred)
[f arg1 arg2]                   # Positional args
[f arg1 opt: val]               # Named args

# Function application — explicit call (for computed functions)
[call [get-handler request] data]   # function from another call
[call % data]                       # pipeline value used as function

# Implicit lambda (_ shorthand — same as $_ before)
[+ _ 1]                         # desugars to [fn [_] [+ _ 1]]
[> _.age 30]                    # desugars to [fn [_] [> _.age 30]]

# Apply (spread list into function args)
[apply f arg-list]              # Spreads list entries as positional args

# Function definition — [let ...] declares bindings
[fn@Number [let x@Number  y@Number]
  [+ x y]]

# Named function (just a dict entry)
add: [fn@Number [x@Number  y@Number]
  [+ x y]]

# Named parameters (Kotlin model: any parameter can be named)
fetch: [fn@String [url@String  timeout@[type: Number  default: 30]]
  ...]

# Variadic parameters
apply-all: [fn [f ...args] [map f args]]

# Type alias
User: [type [name: String  age: Number]]
greet: [fn@String [u@User] [str "Hello " u.name]]

# @ property annotations
param@Type                      # Shorthand: param@[type: Type]
param@[type: T  default: val]   # Full form with properties
fn@Type                         # Return type (shorthand)
fn@[return: T  doc: "..."]      # Return type with properties

# @ type assertions (on expressions)
[@Number expr]                  # Assert type, throw on mismatch
[@[type: Number  default: 0] expr]  # Safe cast with fallback

# Type expressions
[key: Type ...]                 # Open record type
[key: Type]                     # Closed record type
[Type]                          # List type
[Fn@b [a]]                     # Function type (mirrors fn definition)
Any                             # Dynamic escape hatch

# Materialization (explicit, runtime-supported)
[eval %]                        # Recursively force all thunks into memory

# Include
utils: [include "lib/utils.llt"]   # Namespaced
[include "lib/utils.llt"]          # Merged into scope (as top-level expression)

# Conditionals (stdlib functions)
[if cond then else]             # Returns then or else
[when cond body]                # Returns body or [] (expression-safe)
[unless cond body]              # Returns body or [] (expression-safe)

# Pipelines (using _ shorthand for multi-arg functions)
[-> data
    [filter [> _.age 30] _]    # two _ levels: inner = element, outer = collection
    [map _.name _]             # inner _.name = element transform, outer _ = collection
    sort]                      # Already 1-arg, no _ needed

# Comments
# This is a comment
[x: 5]  # Inline comment
```

---

## 2. `[]` — The Only Bracket

**`[]` is the only bracket type.** There are no `()` or `{}` in tinct. Every expression — function calls, data structures, type annotations, control flow — uses `[]`. The meaning is determined by what appears inside, not by which bracket type is used.

```tinct
[key: "value"]           # Dict
[f x y]                  # Function call (bare identifier in head position)
[fn [x] x]               # Function definition
[1 2 3]                  # Sequence
[]                        # Empty dict / empty list
```

**Why single brackets:**

- Simpler — one bracket type, one concept
- `()` and `{}` are both freed for future use
- Bare identifier in head position signals application, approaching Lisp's `(f x y)` ergonomics
- `[]` is familiar from JSON, Python, JavaScript
- True unification: there's one data structure, so there's one syntax

The parser determines how to interpret a `[]` by examining its first entry. An identifier that isn't a keyword → call. A keyword → special form. A keyed entry (`word:`) → dict. A `$`-prefixed identifier in head position → data sequence.

---

## 3. Literals and References

### Strings

String values must be quoted. Tinct supports both double-quoted (`"`) and triple-quoted (`"""`) strings. Bare words are variable references, not strings.

```tinct
"hello"                  # String literal
"has spaces"             # Spaces and special characters require quoting
"true"                   # String — quoting overrides boolean recognition
"42"                     # String — quoting overrides numeric recognition
```

**Escape sequences:** `\"`, `\\`, `\n`, `\t`, `\r`. Unicode escapes (`\uXXXX`) are not supported — use `from-json` for full Unicode string parsing.

**Multi-line strings.** Both `"..."` and `"""..."""` permit literal embedded newlines. Triple-quoted strings also strip indentation automatically, using the closing delimiter line as the baseline:

```tinct
# Double-quoted — indentation is preserved verbatim
query: "SELECT *
  FROM users
  WHERE active = true"
# → "SELECT *\n  FROM users\n  WHERE active = true"

# Triple-quoted — indentation stripped to match closing delimiter
query: """
  SELECT *
  FROM users
  WHERE active = true
  """
# → "SELECT *\nFROM users\nWHERE active = true\n"
```

The closing `"""` on its own line is the indentation anchor — its leading whitespace determines how much is stripped from each content line. Content aligns visually with the surrounding code.

Triple-quoted strings include a trailing newline (the newline before the closing delimiter). To suppress it, compose with `trim`:

```tinct
label: [trim """
  Click here
  """]
# → "Click here"
```

Single `"` and `""` inside triple-quoted content need no escaping. To include a literal `"""`, escape the first quote: `\"\"\"`.

The `unindent` stdlib function is the mechanism behind `"""..."""` and is independently useful for strings read from files or built dynamically:

```tinct
[unindent [slurp template-file]]
[trim [unindent raw-block]]
```

**Interpolated strings.** The `i"..."` and `i"""..."""` prefixes embed variable references directly:

```tinct
i"Hello $name"                    # Desugars to: [tmpl "Hello $name"]
i"Price: $$$amount"               # $$ escapes to literal $ → "Price: $42"
i"""
  Dear $name,
  Your order is ready.
  """                             # Interpolation + indentation stripping
```

Variable names stop at common punctuation (`,`, `.`, `!`, `?`) in addition to whitespace and delimiters. Interpolated strings desugar to `[tmpl ...]` calls at parse time, where the template string encodes interpolations with `$name` placeholders (e.g., `i"Hello $name"` → `[tmpl "Hello $name"]`).

### Numbers

```tinct
42                       # Int
-1                       # Negative int
3.14                     # Float
-0.5                     # Negative float
```

Floats must have digits on both sides of the decimal point. The tokenizer tries float before int so `3.14` is never misread as `3` followed by `.14`.

### Booleans

```tinct
true
false
```

The `!ident_char` lookahead ensures `truename` is a bare word, not `true` followed by `name`.

### Variable References

Bare words are variable references. Any token that isn't a numeric literal, boolean, or quoted string resolves as a reference to a binding in the current scope:

```tinct
x                        # Refers to the binding named "x"
my-key                   # Refers to "my-key"
%config                  # Refers to "%config" (pipeline convention)
日本語                    # Unicode identifiers work
```

String values always require quotes — there is no bare-word string. This makes the rule precise: bare words are always references.

---

## 4. Dict Entries

A dict (dictionary) is the fundamental data structure. Key-value pairs use `:` as the separator. Keys are bare words by default; values are any expression.

```tinct
[key: "value"]                   # Single key-value pair
[name: "Alice"  age: 30]         # Two pairs
[host: "localhost"  port: 8080]  # Strings and numbers
[config: [timeout: 30  retries: 3]]  # Nested dict as value
```

**Positional (auto-indexed) entries.** Entries without a key are auto-indexed starting from `0`:

```tinct
["a" "b" "c"]            # Indices 0, 1, 2
[1 2 3]                  # Indices 0, 1, 2
```

**Mixed ordering.** Positional and keyed entries may appear in any order. Auto-indices are assigned sequentially to positional entries regardless of where keyed entries appear:

```tinct
[a "b" key: "val" c]     # Positional: "a"→0, "b"→1, "c"→2; named: key→"val"
[x: 1 $y]               # Named: x→1; positional: ref(y)→0
```

**Semicolons** are a newline alias — they allow one-line dict literals:

```tinct
[a: 1; b: 2; c: 3]      # Same as three lines with newlines
```

**Value boundary rule.** Every entry's value is exactly one token or one `[]` expression. Whitespace separates entries — the parser never has to guess where one value ends and the next begins:

```tinct
[name: "Alice" age: 30]         # Two entries: name->"Alice", age->30
[key: [$a $b $c]]               # One entry: key->[ref(a), ref(b), ref(c)]
[key: value1 value2 value3]     # THREE entries: key->value1, 0->value2, 1->value3
[key: [value1 value2 value3]]   # One entry: key->[value1, value2, value3]
```

**Computed keys.** A `$`-prefixed key is a computed key — it resolves the reference and uses the result as the key:

```tinct
[$key: val]              # Computed key: resolves key, uses as key
```

---

## 5. Function Calls

### Implied Call (preferred)

Any bare identifier in head position (first slot inside `[]`) is a function call:

```tinct
[f x y]                  # Call f with x and y
[map double data]        # Call map with double and data
[+ 1 2]                  # Call + with 1 and 2
[f]                      # Zero-argument call to f
```

### Explicit `call`

`call` is **mandatory** when the function is a computed expression rather than a bare identifier. Without it, a bracket expression in head position parses as a data sequence, not a call:

```tinct
[call [get-handler request] data]   # Mandatory — [[get-handler request] data] is a data sequence
[call [fn [x] [+ x 1]] 42]         # Mandatory — [[fn [x] [+ x 1]] 42] is a data sequence
```

`call` is **optional** when the function is a bare identifier — it produces the same AST as the implied call:

```tinct
[call f x y]                        # Same as [f x y]
[call % data]                       # Same as [% data]
[call fn-ref arg]                   # Same as [fn-ref arg]
```

### Named Arguments

Any argument can be named using `key: value` syntax inside a call:

```tinct
[f x name: "val"]        # Positional x, named name
[fetch url timeout: 30]  # Named timeout parameter
[f a: 1  b: 2]           # All named
```

Named argument keys are bare identifiers — `$key: val` is not valid in call argument lists.

### `$`-prefixed Head — Data, Not a Call

A `$`-prefixed identifier in head position marks a data sequence, never a call:

```tinct
[$f x y]                 # Data: sequence [ref(f), ref(x), ref(y)]
[$f]                     # Data: single-element sequence [ref(f)]
```

This makes `[$f]` unambiguous — a single-element sequence, not a zero-argument call.

### Special Forms vs Stdlib Functions

Most "control flow" in tinct is stdlib functions, not special forms, because lazy evaluation means the unused branch is never materialized:

```tinct
[if [> x 0] positive non-positive]
[and [valid? input] [process input]]  # process never called if invalid
[or cached-value [expensive-compute]] # compute skipped if cached
```

Only constructs that affect **binding structure** or **dict construction** are special forms built into the parser:

| Special form | Why |
|--------------|-----|
| `call` | Triggers function application |
| `fn` | Introduces parameter bindings, creates a new scope |
| `type` | Compile-time type declaration |
| `let` | Binding declaration list — introduces names in fn params, case, class, and instance contexts |
| `case` | Match arm with explicit pattern scoping (inside `[match ...]`) |
| `class` | Type class declaration |
| `instance` | Type class instance implementation |
| `pattern` | Instance arm pattern declaration |
| `match` | Pattern matching with arm bindings |
| `quote` | Captures AST as data without evaluating |
| `unquote` | Splices values into quoted templates |
| `unquote-splice` | Splices sequence elements into quoted list positions |
| `defmacro` | Registers compile-time AST transformation |
| `macro` | Defines a syntax transformer (compile-time macro) |
| `syntax-class` | Declares a named syntactic pattern class with a diagnostic message |

A keyword followed by `:` is a dict entry, not a special form: `[call: something]` is a dict with key `call`.

---

## 6. Function Definitions

Functions are defined with `fn`. Parameters are always wrapped in a `[let ...]` binding declaration list:

```tinct
[fn [let x] x]                                    # Identity function
[fn [let x y] [+ x y]]                           # Two parameters
[fn@Number [let x@Number  y@Number] [+ x y]]     # With type annotations
[fn [let f ...args] [map f args]]                 # Variadic parameter
```

The `[let ...]` form is the universal binding declaration — it appears in `fn`, `class`, `type`, `instance`, and `case` positions. See §9 Advanced Features for the full specification.

### Parameters and Annotations

Each parameter may have a type annotation using `@`:

```tinct
x@Number                          # Parameter x must be Number
timeout@[type: Number  default: 30]  # Number with a default value
```

The function itself can be annotated for its return type:

```tinct
fn@String                         # Returns String
fn@[return: String  doc: "..."]   # Return type with properties
```

### Variadic Parameters

`...name` captures remaining positional arguments into a list:

```tinct
[fn [let f ...args] [map f args]]
```

At most one variadic parameter is allowed; it must appear last.

### Local Bindings / Sequential

Any `fn` body with multiple expressions forms a sequential scope — each `[name: val]` step extends the environment for all subsequent steps:

```tinct
[transform: [fn [input]
  [cleaned:  [trim input]]
  [parts:    [split ":" cleaned]]
  [str [get 0 parts] "@" [get 1 parts]]]]
```

The bindings `cleaned` and `parts` are local to this fn body. This is the idiomatic tinct approach to local variables.

The same pattern works at document level: each top-level `[name: val]` expression extends the environment for all subsequent expressions in the same document.

---

## 7. Access Chains

Dot access retrieves a field from a dict:

```tinct
person.name                      # Get key "name" from person
config.database.host             # Chained: config → "database" → "host"
data.0                           # Integer dot access (looks up Key::Int(0))
```

Dot access is **whitespace-insensitive** — a space before `.` is allowed and produces the same result:

```tinct
$config
  .database                      # Same as $config.database
  .host                          # Chained: $config.database.host
```

Both bare identifiers (`name.field`) and `$`-prefixed references (`$name.field`) support access chains.

For position-based access, use stdlib functions:

```tinct
[get 0 data]             # Integer key access (key-based)
[nth data 0]             # Position-based: first entry
[nth data -1]            # Position-based: last entry
[slice data 2 5]         # Subsequence by position
```

---

## 8. Annotations

`@` is a structural separator that attaches type information. It appears immediately after a bare word with no space.

### Parameter Annotations

Inside a `fn` parameter list:

```tinct
x@Number                          # Param x with type Number
timeout@[type: Number  default: 30]  # With properties
```

### Return Type Annotation

On the `fn` keyword:

```tinct
[fn@String [x] x]                 # Function returning String
[fn@[return: String  doc: "..."] [x] x]  # With properties
```

### Annotated Values

In any value position:

```tinct
Fn@Number                         # Annotated value: "Fn" with annotation Number
Fn@[Fn@c [b]]                    # Nested: function returning a function type
```

### Parameterized Type Annotations

Bracket application passes type arguments to a type constructor. The annotation contains a bracket expression that calls a type-stage function with type arguments.

```tinct
# Seq — element type via bracket application
xs@[Seq Int]                    # Seq of Int
fn@[Seq String] [s@String]      # Function returning Seq of String

# Map — two annotation forms
scores@[Map [String: Int]]      # "map from String to Int" (lookup perspective)
hosts@[Map T1]                  # "collection of T1 values" (key type: Any)
index@[Map [String: Any]]       # string-keyed, untyped values
cache@Map                       # bare: any map
transitions@[Map [key: Int  value: [Seq Int]]]  # explicit named form

# Records with reserved field names — use a type alias
Config: [type [record host: String  port: Int]]
config@Config                                   # alias for the record type
r@[host: String  port: Int]                     # inline record annotation (no reserved fields)

# Type alias composition
T1: [type [record host: String  port: Int]]
T2: [type [Map T1]]            # Map of T1 values (key: Any) — collection perspective
T3: [type [Map [String: T1]]]  # Map from String to T1 — lookup perspective
process: [fn [hosts@T2] ...]   # alias resolves transitively
```

**Bracket application for type parameters.** In `xs@[Seq Int]`, the `@` is the annotation separator and `[Seq Int]` is the annotation — a type-stage function application. `Seq` is called with `Int` as its element type argument. This is identical to how data bracket expressions work: `[Seq Int]` evaluates `Seq` in the type-stage Env and applies `Int` as an argument.

**`[Map T]` vs `[Map [K: V]]`.** These are two perspectives on the same map type:

- `[Map T]` — collection perspective: iterate the values, key type is `Any`. Useful when you only care about the value type.
- `[Map [K: V]]` — lookup perspective: access by key of type `K`, value type `V`. Useful when the key type matters.

**Records with reserved field names.** The shorthand `x@[field: Type ...]` works when field names are unambiguous. When field names collide with annotation property keys (`type`, `return`, `default`, `doc`, `is`, `repr`, `constraint`, `bind`, `kinds`), define a type alias and annotate with the alias name — `x@[type: String]` is parsed as a property dict (setting the `type` property), not as a Record with a `type` field.

**Bare `@Map`, `@Seq`** are accepted and mean "any map / any sequence" (unconstrained element/key/value types).

**Annotations only.** These parameterized forms are for type annotations and `[type ...]` aliases — they describe types. They do not change how dicts are constructed or accessed at runtime.

### Type Assertions

When `@` is the first token inside `[]`:

```tinct
[@Number expr]                    # Assert expr is Number; error on mismatch
[@[type: Number  default: 0] expr]  # Safe cast: return 0 if type mismatch
```

The annotation value after `@` may be a single word or a full property dict. A space before `@` prevents annotation detection — `word @Annotation` is a bare word followed by a separate expression.

| Input | Interpretation |
|-------|----------------|
| `x@Number` | Annotation: "x" with type Number |
| `fn@String` | fn with return annotation String |
| `Fn@b` | Annotated value: "Fn" annotated with "b" |
| `[@String $x]` | Type assertion expression |
| `"a@b"` | Quoted string "a@b" |
| `x@[Seq Int]` | Annotation: x with type Seq(Int) |
| `x@[Map [String: Int]]` | Annotation: x with type Map(String, Int) |
| `x@[Map T1]` | Annotation: x with type Map(Any, T1) |
| `Config: [type [record host: String  port: Int]]` | Type alias Config = Record{host:String, port:Int} |

---

## 9. Advanced Features

### Type Expressions

Type expressions appear in annotations and `[type ...]` declarations. They use the same `[]` syntax as data.

**Function types** use `Fn@Return [ParamTypes]`, mirroring function definitions:

```tinct
[Fn@b [a]]              # Function from a to b
[Fn@Bool [a]]           # Predicate
[Fn@c [a b]]            # Two-arg function
```

**Type alias.** Bind a name to a type expression using `[type ...]` as a dict value. The alias name is then usable in annotations:

```tinct
# Define aliases
Name: [type String]
Age:  [type Number]
User: [type [name: Name  age: Age]]
Predicate: [type [Fn@Bool [a]]]
Transform:  [type [Fn@b [a]]]

# Use in annotations
greet: [fn@String [u@User] [str "Hello " u.name]]
users: [filter [fn [u@User] [> u.age 18]] all-users]

# Aliases compose
AdminUser: [type [name: Name  age: Age  role: String]]
```

Aliases compose with parameterized type annotations:

```tinct
Config:     [type [record host: String  port: Int]]
Hosts:      [type [Map Config]]           # collection of Config values (key: Any)
NamedHosts: [type [Map [String: Config]]] # lookup: name → Config
```

Type aliases are resolved at type-check time — they have no runtime cost.

**Type constructor application.** Type constructors like `Seq` and `Map` are applied in annotation positions using the syntax:

```tinct
@[Seq Int]        # Apply Seq constructor to Int
@[Map String]     # Apply Map constructor to String
```

The parser produces `Annotation::PropertyDict` for `@[...]` forms. During type checking, `resolve_type_expr` in `src/typecheck_annot.rs` resolves these by pattern-matching on the constructor name (`Seq`, `Map`, etc.) and creating the corresponding `Type::*` variant (`Type::Seq`, `Type::Map`). This happens after parsing completes, when annotations are resolved to concrete types.

Note: The `SurfaceExpression::TypeApp` AST variant exists for legacy reasons but is never constructed by the parser. Type constructor applications are resolved directly during type checking, not during parsing.

**Row polymorphism.** `...` marks an open record type; `...name` introduces a named row variable:

```tinct
[name: String ...]            # Open record: has name, allows other fields
[name: String ...r]           # Named row variable r captures remaining fields
```

**Type conventions** (enforced by type checker, not parser):

- Uppercase first letter = concrete type (`String`, `Number`, `Person`, `Fn`)
- Lowercase first letter = type variable (`a`, `b`, `k`, `v`)
- `Any` = dynamic escape hatch

### Binding Declarations — `[let ...]`

`let` is a keyword. `[let ...]` is a **binding declaration list** — always. Each element is a binding pattern:

```tinct
[let x]             # bare binding
[let x@Int]         # typed binding
[let _]             # wildcard
[let v: Ok]         # structural test — v binds to Ok's payload
[let [a b]: Pair]   # multi-payload structural test
```

`[let ...]` appears as the first expression in `fn`, `class`, `type`, `instance`, and `case` positions. Outside these positions, `[let ...]` is a type error. Every bracket NOT starting with `let` is always an expression — no exceptions.

This invariant is complete: `[a b c]` is always an implied call. A reader never needs to know the enclosing context to determine whether a bracket introduces names or calls a function.

### Match Arms — `[case ...]`

`case` is a keyword. `[case pattern body]` is a match arm with explicit scoping:

```tinct
[match result
  [case [let v: Ok]   v]           # structural test; v scoped to body
  [case [let e: Err]  [log e]]     # structural test; e scoped to body
  [case [let _]       0]]          # wildcard

[match status
  [case 200           "ok"]        # exact value — no [let ...], no new bindings
  [case 404           "missing"]
  [case [let n@Int]   [str n]]]    # typed binding
```

Existing match arm shorthands (`[Ok v]:`, `n@Int:`, `_:`) coexist with `[case ...]`. Migration to `[case ...]` is encouraged.

### Placeholder Expression — `...`

`...` (three dots, no trailing identifier) is a placeholder expression. It produces a lazy thunk that raises `UnimplementedError` with its source span when materialized:

```tinct
process: [fn@Result [let data@Input] ...]    # stub — type-checks, fails at call time

config: [host: "localhost"  port: ...]        # required but unset — fails when accessed

[match x
  [case [let v: Ok]  v]
  [case [let _: Err] ...]]                   # unreachable branch guard
```

`...` has type `Unknown` — the gradual escape hatch. It satisfies any type constraint without generating a type error. `UnimplementedError` is catchable via `$try`.

**Disambiguation from other `...` uses:** `...rest` in a parameter list is a variadic binding. `[name: Str ...]` in a type annotation is an open record rest. Bare `...` (no following identifier) in value position is the placeholder expression.

### Pattern Matching

```tinct
[match x
  [case 0           "zero"]
  [case 1           "one"]
  [case [let _]     "other"]]

[match response
  [case [let result: Ok]  result]
  [case [let msg: Err]    [error msg]]]
```

Patterns include wildcards, variable bindings, literals, type tags, pins, and dict/list destructuring. See the `[case ...]` section above for the full binding pattern specification.

### Quasiquoting

`quote`, `unquote`, and `unquote-splice` treat code as data. `[quote expr]` produces an AST dict.

```tinct
[ast: [quote [+ 1 2]]]    # → [type: "call"  fn: "+"  args: [1 2]]
ast.fn                     # → "+"

# Splice a runtime value into a quoted expression
[op: "+"]
[quote [[unquote op] 1 2]] # → [type: "call"  fn: "+"  args: [1 2]]

# Round-trip: quote then evaluate using eval builtin
[eval [seq [quote [+ 1 2]]]] # → 3

# Build and run code programmatically
[template: [fn [op a b] [eval [seq [quote [[unquote op] [unquote a] [unquote b]]]]]]]
[template "+" 10 20]        # → 30
```

`[unquote expr]` is valid only inside `[quote ...]`. `[unquote-splice expr]` splices a sequence into a list position inside `[quote ...]`.

### Macro Definition

`[macro name [let params] body]` registers a compile-time AST transformation. `[defmacro ...]` is a backward-compatible alias for `[macro ...]`.

```tinct
[macro my-when [let pred body]
  [quote [if [unquote pred] [unquote body] []]]]

[my-when [> x 0] [process x]]  # expands to: [if [> x 0] [process x] []]
```

Macro names cannot shadow registered Rust builtins. See `doc/08-evaluation.md` §Macro Expansion Pipeline for expansion semantics.

### `class` and `instance`

Type class declaration:

```tinct
[class [Eq a]
  eq: [Fn@Bool [a a]]]

[class [Ord a] [Eq a]
  lt: [Fn@Bool [a a]]
  gt: [Fn@Bool [a a]]]
```

Type class instance:

```tinct
[instance [Eq Int]
  eq: [fn [x y] [= x y]]]

[instance [Eq [name: String age: Int]]
  eq: [fn [a b] [and [= a.name b.name] [= a.age b.age]]]]
```

---

## 10. Document Structure

A tinct **file** contains one or more **documents** separated by `---`. Each document contains one or more **expressions**. Sequential expressions form a scope chain — each expression's result becomes the parent scope for the next.

```tinct
[x: 10]                          # Expression 1
[y: [+ x 1]]                    # Expression 2 (sees x from parent scope)
---
[z: %.x]                        # New document (% carries previous output)
```

**Documents are isolated.** The only connection between documents is `%`, which carries the previous document's output as a lazy value. For the first document, `%` is `[]`.

**An empty document** (zero expressions) produces an empty dict `[]`. An empty file produces a file with one document containing zero expressions.

### Document Separator / Section Headers

The `---` line may carry optional section header components:

```text
--- %name@Type expects: Type caps: [%cap: @CapType]
```

All components are optional. A bare `---` is valid. Components may appear in any order:

| Component | Syntax | Purpose |
|-----------|--------|---------|
| Section name | `%name` | Binds the document's output as `%name` for subsequent documents |
| Output type annotation | `@Type` | Declares the type of this document's output |
| Input contract | `expects: Type` | Declares the expected type of `%` |
| Capability requirements | `caps: [%cap: @Type ...]` | Declares required capability bindings |
| Evaluation stage | `stage: type` | Marks the document as a type-stage document (evaluated during type checking, not at runtime); only `type` is a valid value |

```tinct
---                                    # bare separator
--- %config                            # named section
--- %validated@Config                  # named + output type
--- expects: InputSchema               # input contract
--- %result@Result expects: @Input caps: [%nc: @NetCap]   # all components
```

See `doc/09-documents.md` for detailed semantics of section headers, pipeline flow, and capability injection.

---

## 11. Lexical Details

### Whitespace and Comments

`#` to end of line — Python/shell style. No block comments.

```tinct
# This is a comment
[x: 5  y: 10]  # Inline comment
```

Whitespace and comments are implicitly skipped between tokens.

### Whitespace Significance

Whitespace is significant only for `@` (annotation), not for `.`:

- `a.b` — dot access
- `a .b` — also dot access (whitespace before `.` is allowed)
- `word@Annotation` — annotation (no whitespace before `@`)
- `word @Annotation` — bare identifier `word` followed by separate expression

`@` (ImmediateAt) is the only whitespace-sensitive token. A space before `@` prevents annotation detection. `.` is not whitespace-sensitive — dot access works with or without preceding whitespace.

### Identifier Character Rules

Identifiers use a **denylist** approach — any character is valid except structural delimiters:

| Excluded character | Purpose |
|--------------------|---------|
| Whitespace | Token separator |
| `[` `]` | Bracket expressions |
| `:` | Key-value separator |
| `;` | Entry separator / newline alias |
| `#` | Comment start |
| `"` | String literal start |
| `@` | Annotation separator |
| `.` | Dot access (so `a.b` is access, not one identifier) |
| `\|` | Pipe operator |

This provides extensibility for new operators without reserved keywords, and enables full Unicode identifier support. Identifiers cannot start with `$`, `@`, `#`, `[`, `]`, `:`, `;`, `"`, or `...`.

**`$` (escaped reference)** follows the same character rules and prefixes an identifier: `$name`, `$+`, `$>=`, `$->`.

**`%` identifiers** (`%config`, `%base`) are ordinary identifiers used by convention for pipeline references. The lexer handles `%name` so that `%base.x` tokenizes as `Identifier("%base")`, `Dot`, `Identifier("x")`.

**Dot in identifiers.** `.` is excluded from identifier characters, so `file.txt` tokenizes as `Identifier("file")`, `Dot`, `Identifier("txt")`. Values containing dots that are not intended as access chains must be quoted: `"file.txt"`.

**Unicode homograph risk.** Unicode homographs (e.g., Cyrillic `а` vs Latin `a`) create invisible name collisions; tinct accepts all Unicode identifier characters without NFC normalization.

### Token Precedence

When classifying a bare token, the tokenizer applies rules in this order:

1. `$` sigil → escaped reference
2. Numeric → float (tried first) or int
3. `true`/`false` → boolean
4. `"` → quoted string
5. Followed immediately by `@` (no whitespace) → annotated value
6. Everything else → identifier (variable reference)

### `$` Disambiguation

`$` is a position-dependent disambiguator, not a universal reference sigil:

| Position | Default interpretation | `$` overrides to |
|----------|----------------------|------------------|
| Key (before `:`) | String key | Computed key (reference) |
| Head (first in `[]`) | Call (when bare identifier) | Data — NOT a call |
| Other value | Reference | Reference (redundant, harmless) |

```tinct
[f x y]              # Call: f(x, y) — bare identifier in head
[$f x y]             # Data: sequence [ref(f), ref(x), ref(y)] — $-head prevents call
[$key: val]          # Computed key: resolves key, uses as key
[f $x y]             # Call f(x, y) — $x and x are identical in non-head position
```

---

## 12. Complete Grammar

**Canonical Source:** The authoritative parser implementation is in `src/parser.rs` (hand-written iterative descent parser) and `src/lexer.rs` (hand-written tokenizer). The EBNF notation in this section is illustrative and documents the language grammar — it is not executable.

```ebnf
// === Whitespace and Comments ===

WHITESPACE = " " | "\t" | "\r" | "\n"
COMMENT    = "#" ~ (!NEWLINE ~ ANY)* ~ (NEWLINE | EOI)

// === File and Document Structure ===

file          = SOI ~ document ~ (section_header ~ document)* ~ EOI
document      = expression*
expression    = !section_header ~ value
section_header = "---" ~ !ident_char_body ~ header_components? ~ NEWLINE
header_components = header_component+
header_component  = section_name | output_annotation | expects_pragma | caps_pragma
section_name      = "%" ~ ident_char+
output_annotation = "@" ~ annotation_value
expects_pragma    = "expects:" ~ "@"? ~ annotation_value
caps_pragma       = "caps:" ~ "[" ~ cap_entry* ~ "]"
cap_entry         = "%" ~ ident_char+ ~ ":" ~ "@" ~ ident_char+

// === Bracket Expressions ===

bracket_expr = "[" ~ "]"
             | "[" ~ type_assert_body ~ "]"
             | "[" ~ special_form ~ "]"
             | "[" ~ call_implied ~ "]"
             | "[" ~ dict_entries ~ "]"

// === Special Forms ===

special_form = call_form | fn_form | type_form

call_form    = keyword_call ~ value ~ call_args
call_implied = identifier ~ call_args     // identifier not a keyword, not followed by ":"
fn_form      = keyword_fn ~ fn_annotation? ~ param_list ~ value
type_form    = keyword_type ~ value

keyword_call = "call" ~ !ident_char ~ !colon_ahead
keyword_fn   = "fn" ~ !ident_char ~ !colon_ahead
keyword_type = "type" ~ !ident_char ~ !colon_ahead

// Lookahead: optional horizontal whitespace then colon.
// ws_chars matches only spaces and tabs (not newlines), so "call\n:" is a Call expression, not a Dict entry.
colon_ahead = ws_chars* ~ ":"
ws_chars    = " " | "\t"

ident_char = !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "." | "|") ~ ANY

call_args = (named_arg | value)*

named_arg     = named_arg_key ~ ":" ~ value
named_arg_key = identifier

// === Type Assertions ===

type_assert_body = "@" ~ annotation_value ~ value

// === Functions ===

fn_annotation = "@" ~ annotation_value

param_list = "[" ~ (variadic_param | param)* ~ "]"

param = param_name ~ param_annotation?

param_name = (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-")* ~ "?"?

param_annotation = "@" ~ annotation_value

variadic_param = "..." ~ param_name

// Annotation value allows whitespace inside property dicts like [type: Number default: 30]
annotation_value = bracket_expr | annotation_word

annotation_word = (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-")* ~ "?"?

// === Dict Entries ===

dict_entries = (entry ~ ";"?)*

entry = keyed_entry | rest_entry | auto_entry

rest_entry = "..." ~ annotation_word?

keyed_entry = key ~ ":" ~ value
auto_entry  = value

key = bracket_expr | escaped_ref | quoted_string | bare_token

bare_token = float_lit | int_lit | bool_lit | identifier

// === Values ===

value = access_expr | bracket_expr | atom

atom = float_lit | int_lit | bool_lit | quoted_string | escaped_ref | annotated_bare | identifier

// === Generalized Annotations ===

annotated_bare = identifier ~ "@" ~ annotation_value

// === Access Chains ===

access_expr = (identifier | escaped_ref) ~ access_chain+

access_chain = dot_access

dot_access = "." ~ access_field

access_field = (ASCII_ALPHA | "_") ~ (ASCII_ALPHANUMERIC | "_" | "-" | "?")*

// === Literals ===

escaped_ref = "$" ~ esc_ident

esc_ident = esc_ident_char+

esc_ident_char = !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "." | "|") ~ ANY

float_lit = "-"? ~ ASCII_DIGIT+ ~ "." ~ ASCII_DIGIT+

int_lit = "-"? ~ ASCII_DIGIT+

bool_lit = ("true" | "false") ~ !ident_char

quoted_string = "\"" ~ inner_string ~ "\""
inner_string  = (escape_seq | !("\"" | "\\") ~ ANY)*
escape_seq    = "\\" ~ ("\"" | "\\" | "n" | "t" | "r")

interpolated_string = "i" ~ "\"" ~ interp_part* ~ "\""
triple_interpolated = "i" ~ "\"\"\"" ~ interp_part* ~ "\"\"\""
interp_part         = literal_text | "$" ~ identifier | "${" ~ expression ~ "}"

identifier = ident_start ~ ident_cont*

ident_start = !( "$" | "#" | "[" | "]" | ":" | ";" | "\"" | "@" | "|"
               | " " | "\t" | "\r" | "\n"
               | "..." )
              ~ ident_char_body

ident_cont = !( " " | "\t" | "\r" | "\n"
              | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "|" )
             ~ ident_char_body

ident_char_body = !(WHITESPACE | "[" | "]" | ":" | ";" | "#" | "\"" | "@" | "$" | "|") ~ ANY
```

### Parser Bracket Priority Table

The parser determines how to interpret a `[]` by examining its first entry:

| Priority | Condition | Interpretation | Example |
|----------|-----------|----------------|---------|
| 1 | Empty `[]` | Empty dict | `[]` |
| 2 | `@` first | Type assertion | `[@Type expr]` |
| 2b | Identifier + `@` immediately (annotated identifier), not followed by `:` | Dict | `[Fn@Number [Int]]` |
| 3 | Keyword in head (`call`, `fn`, `type`), not followed by `:` (horizontal) | Special form | `[fn [x] x]` |
| 4 | First entry is keyed (head followed by `:` with no intervening newline) | Dict | `[name: "Alice"]` |
| 5 | Identifier in head (not keyword) | Implied call | `[f x y]` → `f(x, y)` |
| 6 | `$`-prefixed head (`escaped_ref`) | Data sequence | `[$f x y]` |
| 7 | Literal in head | Data sequence | `[1 2 3]` |

**`[f]` is a zero-argument call** to `f` (Priority 5). To construct a single-element data sequence containing a reference, use `[$f]` (Priority 6).

**Newline before colon breaks Priority 4.** `[name\n: val]` is not a keyed entry — the colon lookahead only checks horizontal whitespace (spaces and tabs).

**Horizontal-only lookahead:** The parser's `peek_next_horizontal` skips horizontal whitespace (spaces and tabs) and `Token::Comment(_)` tokens, but stops immediately at both `Token::Newline` and `Token::Semicolon` — neither is skipped. So `[call\n: x]` and `[call;: x]` both parse as call forms, not dict entries.

**Why parser-level:** The distinction between calls and data must be unambiguous before evaluation. The head-position rule classifies brackets at parse time, before any thunks are created.

```tinct
[f x]                 # Call — implied call: bare identifier "f" in head
[f x y]              # Call — call f(x, y)
[call f x]           # Call — explicit call (identical AST to implied)
[fn [x] x]           # Fn
[type [Fn@b [a]]]    # TypeAlias

[call: something]    # Dict — "call" followed by ":" is a key, not a keyword
[$f x y]             # Dict (data sequence) — $-prefixed head prevents call
[f]                  # Call — zero-argument call to f
[$f]                 # Dict — single-element sequence containing ref(f)
[call
: value]             # Call — newline breaks colon_ahead (it only matches spaces/tabs, not newlines)
```

---

## 13. Token Disambiguation

| Input | Interpretation | Rule |
|-------|---------------|------|
| `name` | Variable reference | Identifier (fallback) |
| `$name` | Escaped reference (disambiguator) | `$` sigil |
| `42` | Int literal | Numeric before identifier |
| `3.14` | Float literal | Float before int |
| `true` | Bool literal | Bool before identifier |
| `"hello"` | Quoted string literal | Quote delimited |
| `"true"` | Quoted string (value `true`) | Quoting overrides |
| `hello` | Reference to `hello` | Identifier |
| `[f x]` | Call `f(x)` | Identifier in head, not keyword |
| `[$f x]` | Data sequence `[ref(f), ref(x)]` | Escaped ref in head |
| `[f]` | Zero-arg call `f()` | Identifier in head |
| `[$f]` | Single-element sequence `[ref(f)]` | Escaped ref in head |
| `a.b` | Dot access | Dot access; whitespace-insensitive (same as `a .b`) |
| `a .b` | Dot access (same as `a.b`) | Dot is whitespace-insensitive |
| `a[0]` | Two separate expressions | Bare identifier `a`, then dict `[0: 0]` — use `[get 0 a]` for key lookup |
| `a [0]` | Two separate expressions | Bare identifier `a`, then dict `[0: 0]` |
| `$a.b` | Access chain | Dot is whitespace-insensitive; same as `$a .b` |
| `$a [0]` | Two separate expressions | Escaped ref `$a`, then dict `[0: 0]` |
| `x@Number` | Param with annotation | `@` in param context |
| `fn@String` | fn with return annotation | `@` after `fn` keyword |
| `Fn@Number` | Annotated value | `@` in value context |
| `[@T e]` | Type assertion | `@` first in `[]` |
| `call` (first in `[]`) | Keyword | Special form recognition (Priority 3) |
| `call@Type` (first in `[]`) | Annotated (NOT keyword) | `@` after identifier converts keyword candidate |
| `call:` | Dict key | Colon makes it a key |
| `%config` | Reference to pipeline section `config` | Identifier with `%` prefix (convention) |
| `---` (between exprs) | Document separator | `doc_separator` rule |
| `----` | Identifier | `!ident_char` prevents separator match |
