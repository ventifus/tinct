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

**Type-theoretic implication:** The static `Record` type tracks only string-keyed fields; integer-keyed (positional) entries are not part of the record type. A dict `[a b c  name: Alice]` has record type `[name: String]` — the positional entries `a`, `b`, `c` are invisible to the type checker. This is a deliberate consequence of unifying lists and records: positional entries are list-like data without static field names, while named entries form the record structure that type inference reasons about.

### Principle 2: One Bracket, One Structure

**`[]` is the only bracket type.** There is one syntax for the one fundamental data structure. Entries with `key:` are keyed; entries without get auto-incrementing integer keys. Both can appear in the same `[]`.

```lisp
[name: Alice  age: 30]          # All keyed — a "dict"
[a b c]                         # All auto-indexed — a "list" = [0: a  1: b  2: c]
[call $f $x timeout: 60]        # Mixed — positional + named
[]                              # Empty — list and dict are identical
```

**Parsing rule:** After parsing an entry, look ahead for `:`. If found, the entry is a key and the next thing is its value. If not, the entry is auto-indexed. The integer counter only increments for unkeyed entries — keyed entries don't consume an index.

**Positional and named entries may appear in any order.** Auto-indices are assigned sequentially to positional entries regardless of where named entries appear. For function calls, the binding priority chain (§Call Convention, C-PRIORITY) resolves positional arguments by index, then named arguments fill remaining parameters, then defaults apply.

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

**Mandatory, bottom-up type inference with annotation-driven polymorphism, inspired by Hindley-Milner.** Every value has a type. Row polymorphism for dicts. Type errors raised early — good for LLMs and LSP feedback. Let-generalization uses levels-based approach (Kiselyov 2013) for polymorphic let-bindings — see §Let-Generalization. Polymorphism arises from type variable annotations (e.g., `x@a`); let-generalization makes these polymorphic across binding sites. See the Formal References section for details.

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

**Any parameter is nameable at the call site** (Kotlin model). A parameter with `default:` is optional — it uses the default value when neither a positional nor named argument covers it. A parameter without `default:` is required — it must be covered by either a positional argument at its index or a named argument. Required and optional parameters may be freely interleaved in the parameter list.

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

**Substitution idempotence invariant.** `Substitution::apply()` is idempotent: applying the same substitution twice yields the same result as applying it once. This is achieved by transitive chasing in `apply_inner()` — when resolving a type variable, the substitution follows the binding chain to a fixpoint rather than performing a single lookup. This guarantees that `apply(apply(ty, s), s) = apply(ty, s)` for all types `ty` and substitutions `s`, which is a standard requirement for unification-based type inference (Robinson, 1965).

**Alpha-equivalence and variable naming.** Variable names are significant in tinct — `[fn [x] $x]` and `[fn [y] $y]` are not alpha-equivalent at the source level. The type checker uses string-based variable lookup, so type variables introduced by annotations bind by name. However, `instantiate()` performs alpha-renaming by generating fresh names (`_t0`, `_t1`, ...) for each call site, ensuring that polymorphic function types do not share type variables across independent call sites. This is a deliberate choice: source-level names matter for readability and error messages, while inference-time freshening prevents unintended unification between call sites.

**Type alias entries are excluded from record fields.** A `[type ...]` entry registers an alias in the type environment but does not contribute a field to the enclosing record's type. This matches the evaluator, which returns an empty dict for type alias entries.

**Function type param lists:** `[Fn@Return [ParamTypes]]` is the full function type syntax. The type checker resolves both the return type annotation and parameter type list, producing `Type::Function { params, ret }`. All types in the param list must be specified explicitly.

### Type Inference Algorithm

This section formally specifies tinct's type inference. The notation uses standard PL conventions: Γ for type environments, ⊢ for typing judgments, S for substitutions, and τ, σ for types.

#### Type Grammar

```
τ ::= IntLiteral(n)              literal integer type
    | StringLiteral(s)           literal string type
    | Int                        integer
    | Float                      float
    | Number                     numeric supertype of Int and Float
    | Str                        string
    | Bool                       boolean
    | Fn(τ₁...τₙ → τᵣ)          function (n params, return type)
    | Seq(τ)                     lazy sequence
    | Record(f₁:τ₁...fₙ:τₙ, ρ)  record with row rest ρ
    | α                          type variable
    | Any                        dynamic/unknown type

ρ ::= Closed                     no additional fields
    | Open                       arbitrary additional fields
    | RowVar(r)                  named row variable (see §Row-Variable Unification)
```

#### Bidirectional Typing

**Not yet implemented — proposed design.**

Tinct uses bidirectional type checking (Pierce & Turner 2000; Dunfield & Krishnaswami 2021) to cleanly separate type inference from subtyping. Two modes:

- **Synthesis (⇒):** `Γ ⊢ e ⇒ τ` — infer the type of e bottom-up (what `infer_expr` does today).
- **Checking (⇐):** `Γ ⊢ e ⇐ τ` — verify e is compatible with expected type τ, using subsumption.

The **subsumption rule** bridges them:

```
Γ ⊢ e ⇒ σ,  σ <: τ
────────────────────────────────── [SUB]
Γ ⊢ e ⇐ τ
```

If an expression synthesizes type σ and σ is a subtype of the expected type τ, then checking succeeds. This is where singleton literal type promotion happens: `42 ⇒ IntLiteral(42)`, and `IntLiteral(42) <: Int`, so `42 ⇐ Int` succeeds. But `Int ≮: IntLiteral(42)`, so checking an `Int`-typed expression against `IntLiteral(42)` fails. Direction matters — subtyping is asymmetric by design.

Note: tinct's `IntLiteral(42)` and `StringLiteral("hello")` are **singleton literal types** — distinct types that are subtypes of their base types (`Int`, `Str`). These are not refinement types in the Dunfield & Pfenning (2004) sense (which use predicate logic, e.g., `{x: Int | x = 42}`). The singleton type approach is simpler and sufficient for tinct's needs; D&P's framework validates that subtyping with type refinements is sound in a bidirectional setting.

Implementation:

```rust
fn check_expr(
    expr: &Spanned<Expr>,
    expected: &Type,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<(), Vec<TypeError>> {
    let actual = infer_expr(expr, env, state, type_map)?;
    if !Type::is_subtype(&actual, expected) {
        Err(vec![TypeError::type_mismatch(expected, &actual, expr.span)])
    } else {
        Ok(())
    }
}
```

`check_expr` is used only at positions where the expected type is fully concrete (no type variables): CALL-MONO arguments, return annotations, and TypeAssert. For CALL-POLY arguments (where type variables need binding), unification with subsumptive fallback is used instead — see [U-SUBSUME] in the Unification section.

**Checking positions** (expected type fully concrete, uses `check_expr` with [SUB]):

| Position | Expected type | Currently uses |
|----------|--------------|----------------|
| Function arguments (CALL-MONO) | Parameter type | Skipped (Limitation #7) → now `check_expr` |
| Function body with return annotation | Declared return type | `is_subtype` → now `check_expr` |
| TypeAssert inner expression | Annotated type | `is_subtype` → now `check_expr` |

**Unification positions** (type variables present, uses `unify` with [U-SUBSUME]):

| Position | Expected type | Mechanism |
|----------|--------------|-----------|
| Function arguments (CALL-POLY) | Instantiated param type (has type vars) | `unify` with [U-SUBSUME] fallback |

**Synthesis positions** (type flows up, no expected type):

| Position | Synthesizes |
|----------|-------------|
| Literals | `IntLiteral(n)`, `Float`, `Bool`, `StringLiteral(s)` |
| Variable references | Instantiated type scheme (VAR-POLY) |
| Dict values | Record type from letrec inference |
| Function definitions | `Fn(params → ret)` |
| Access chains (dot, bracket, range) | Field type or `Any` |

**Confluence.** CALL-POLY uses unification (not `check_expr`) for argument checking because type variables need binding. After substitution application resolves a type variable to a concrete type, subsequent unification attempts against that concrete type use [U-SUBSUME] — a bidirectional subsumption fallback that checks `is_subtype` in both directions. This ensures argument ordering does not affect whether type checking succeeds. See the Unification section for details.

#### Inference Judgments: Γ ⊢ e ⇒ τ

**Literals:**

```
────────────────────────────────── [INT]
Γ ⊢ n : IntLiteral(n)

────────────────────────────────── [FLOAT]
Γ ⊢ f : Float

────────────────────────────────── [BOOL]
Γ ⊢ b : Bool

────────────────────────────────── [STR]
Γ ⊢ "s" : StringLiteral("s")
```

**Variable reference:**

```
Γ(x) = τ
────────────────────────────────── [VAR]
Γ ⊢ $x : τ
```

**Dict (letrec, four-pass):**

Dicts are inferred in four sequential passes over entries e₁...eₙ:

```
Pass 0 — Key resolution: For each entry, determine the string
         key name. Literal string keys extracted directly. Literal
         integer keys converted to string (n.to_string()). Computed
         keys ($k) resolved via Γ: if Γ ⊢ $k : StringLiteral(s),
         key is s; if Γ ⊢ $k : IntLiteral(n), key is n.to_string().
         If non-literal type, entry excluded from Record type.
         Unkeyed positional entries get auto-index keys "0", "1", ...

Pass 1 — Bind all: Γ' = Γ, k₁:Any, k₂:Any, ..., kₙ:Any

Pass 2 — Type aliases: For each [type ...] entry, register the
         alias in Γ'. Each alias sees previously registered siblings.

Pass 3 — Infer values: For each non-alias entry with key kᵢ,
         infer Γ' ⊢ eᵢ : τᵢ. Build Record(k₁:τ₁...kₙ:τₙ, Closed).

Forward references resolve to Any (from Pass 1 bindings).
────────────────────────────────── [DICT]
Γ ⊢ [k₁:e₁ ... kₙ:eₙ] : Record(k₁:τ₁...kₙ:τₙ, Closed)
```

**Function definition:**

```
For each param pᵢ:
    if variadic (...pᵢ): σᵢ = Record([], Closed)   (see Limitation #8)
    else if annotated pᵢ@σᵢ: use σᵢ
    else: σᵢ = Any
Γ' = Γ, p₁:σ₁, ..., pₙ:σₙ
If return annotation @σᵣ given:
    Γ' ⊢ body ⇐ σᵣ                                  [checking mode]
    use σᵣ as return type.
Else:
    Γ' ⊢ body ⇒ τᵣ                                   [synthesis mode]
────────────────────────────────── [FN]
Γ ⊢ [fn@σᵣ [p₁@σ₁ ... pₙ@σₙ] body] : Fn(σ₁...σₙ → σᵣ)
```

Unannotated non-variadic params get type Any. This is the source of the "Any escape hatch" — without annotations, functions have monomorphic type Fn(Any...Any → τᵣ). Polymorphism requires explicit type variable annotations (e.g., `x@a`).

When a return annotation is present, the body is **checked** against it (⇐ mode): the body is synthesized, then subsumption verifies the inferred type is a subtype of the declared type. This replaces the previous `is_subtype` check with the unified bidirectional mechanism.

**Function call (bidirectional):**

Three rules depending on the function type. Arity is always checked.

```
Γ ⊢ f ⇒ Fn(σ₁...σₙ → σᵣ),  has_type_vars(Fn(...)) = false
Γ ⊢ aᵢ ⇐ σᵢ  for i = 1..n                         [checking mode]
|args| = |params|
────────────────────────────────── [CALL-MONO]
Γ ⊢ [call f a₁...aₙ] ⇒ σᵣ
```

Monomorphic path with checking: each argument is **checked** against its parameter type using subsumption. This catches type errors that the previous implementation missed (former Limitation #7): `[call $add "hello"]` where `$add : Fn(Int Int → Int)` now produces a type error because `String ≮: Int`. The `check_expr` call synthesizes the argument type and applies `[SUB]`.

```
Γ ⊢ f ⇒ Fn(σ₁...σₙ → σᵣ),  has_type_vars(Fn(...)) = true
Γ ⊢ aᵢ ⇒ τᵢ  for i = 1..n                          [synthesis]
|args| = |params|
(σ'₁...σ'ₙ → σ'ᵣ) = instantiate(σ₁...σₙ → σᵣ)
S = unify(σ'₁ ≐ τ₁, ..., σ'ₙ ≐ τₙ)                  [with U-SUBSUME]
────────────────────────────────── [CALL-POLY]
Γ ⊢ [call f a₁...aₙ] ⇒ S(σ'ᵣ)
```

Polymorphic path with unification: arguments are **synthesized** (not checked), then unified against instantiated parameter types. Unification binds type variables via [U-VAR] and handles concrete-type comparisons via [U-SUBSUME] (bidirectional subsumption fallback). This is critical for confluence: when multiple arguments constrain the same type variable with different precision (e.g., `IntLiteral(42)` and `Int`), the subsumptive fallback ensures type checking succeeds regardless of argument order. See the Unification section for [U-SUBSUME] details.

Note: CALL-POLY does NOT use `check_expr` because type variables require binding via unification. `check_expr` is reserved for fully concrete expected types (CALL-MONO, TypeAssert, return annotations).

```
Γ ⊢ f ⇒ Any
────────────────────────────────── [CALL-ANY]
Γ ⊢ [call f a₁...aₙ] ⇒ Any
```

Calling a value typed as Any returns Any. Arguments are still synthesized (for type map population and nested error detection) but not checked against parameter types.

Named arguments are type-checked (their values synthesized) but not checked against parameters. Named arg checking requires extending `Type::Function` to carry param names (planned).

**Access chains:**

```
Γ ⊢ e : Record(... k:τ ..., ρ)
────────────────────────────────── [DOT]
Γ ⊢ e.k : τ

Γ ⊢ e : Record(F, Open | RowVar(_)),  k ∉ F
────────────────────────────────── [DOT-OPEN]
Γ ⊢ e.k : Any

Γ ⊢ e : Record(F, ρ),  Γ ⊢ key : StringLiteral(k),  F(k) = τ
────────────────────────────────── [BRACKET-LIT]
Γ ⊢ e[key] : τ

Γ ⊢ e : Record(F, ρ),  Γ ⊢ key : Str | Int | Any
────────────────────────────────── [BRACKET-DYN]
Γ ⊢ e[key] : Any

Γ ⊢ e : Record(F, ρ),  bounds : Int | Str | Any
────────────────────────────────── [RANGE]
Γ ⊢ e[start..end] : Record(F, ρ)

Γ ⊢ e : Any
────────────────────────────────── [ACCESS-ANY]
Γ ⊢ e.k : Any,  Γ ⊢ e[key] : Any,  Γ ⊢ e[start..end] : Any
```

**Type assertion (checking mode):**

```
resolve(ann) = σ,  Γ ⊢ e ⇐ σ                       [checking mode]
────────────────────────────────── [ASSERT]
Γ ⊢ [@σ e] ⇒ σ

resolve(ann) = σ,  Γ ⊢ e ⇐ σ fails,  default ∈ ann
────────────────────────────────── [ASSERT-DEFAULT]
Γ ⊢ [@[type: σ  default: d] e] ⇒ σ
```

Type assertions use checking mode: the inner expression is checked against the annotated type via [SUB]. When checking fails and a `default:` property is present, the assertion succeeds silently (no type error). The default value provides a fallback at runtime.

**Type alias:**

```
resolve(inner) = τ,  register alias in Γ
────────────────────────────────── [ALIAS]
Γ ⊢ [type inner] : Any
```

**Annotated expression:**

```
resolve(ann) = τ
────────────────────────────────── [ANNOTATED]
Γ ⊢ name@ann : τ
```

When name = "Fn": interpret as function type constructor.

**Seq types:** `Seq(τ)` exists in the type grammar and is handled by unification and subtyping, but no user-facing expression directly produces a Seq type. Sequences are created by built-in functions (`$seq`, `$range`, `$map` on seqs, etc.) which are currently typed as Any. Seq type inference for builtins is planned (see TODO.md type-extensions).

#### Unification: unify(τ₁, τ₂, S) → S'

Unification finds a most general substitution S such that S(τ₁) = S(τ₂). Before matching, both types are normalized via S (substitution applied). Unification is **pure Robinson** — it handles type variable binding and structural decomposition only. Subtyping (literal promotion, numeric widening) is handled by `check_expr` via the `[SUB]` rule and `is_subtype`. This separation follows Pierce & Turner (2000) and Dunfield & Krishnaswami (2021).

```
unify(τ, τ, S) = S                              [U-REFL]
unify(Any, τ, S) = S                             [U-ANY-L]
unify(τ, Any, S) = S                             [U-ANY-R]
unify(α, τ, S) = S[α ↦ τ]   if α ∉ FV(τ)       [U-VAR-L]
unify(τ, α, S) = S[α ↦ τ]   if α ∉ FV(τ)       [U-VAR-R]
```

Literal identity (same literal value = same type):

```
unify(IntLiteral(m), IntLiteral(n), S) =
    S           if m = n                         [U-INTLIT-EQ]
    error       if m ≠ n                         [U-INTLIT-NEQ]

unify(StringLiteral(s), StringLiteral(t), S) =
    S           if s = t                         [U-STRLIT-EQ]
    error       if s ≠ t                         [U-STRLIT-NEQ]
```

No explicit literal-to-parent promotion rules in unification. The previous bidirectional silent coercion rules (`[U-INTLIT-UP]`, `[U-INTLIT-DN]`, `[U-INT-NUM]`, `[U-NUM-INT]`, `[U-FLT-NUM]`, `[U-NUM-FLT]`, `[U-INTLIT-FLT]`, `[U-FLT-INTLIT]`, `[U-STRLIT]`, `[U-STR-STRLIT]`) are all removed. Subtyping relationships between concrete types are handled by [U-SUBSUME] below.

Structural:

```
unify(Fn(p₁...pₙ → r₁), Fn(q₁...qₙ → r₂), S) =
    let S' = unify(p₁,q₁, ... pₙ,qₙ, S)
    unify(r₁, r₂, S')                           [U-FN]
    error if |p| ≠ |q|

unify(Seq(τ₁), Seq(τ₂), S) = unify(τ₁, τ₂, S)  [U-SEQ]

unify(Record(F₁,ρ₁), Record(F₂,ρ₂), S) =
    If both Closed: require keys(F₁) = keys(F₂)
        (exact key set match — neither side may have extra fields)
    If either is Open or RowVar: no key-set check performed
        (non-shared fields accepted without type checking)
    For each key k ∈ keys(F₁) ∩ keys(F₂):
        unify(F₁(k), F₂(k), S)
    Row variables are NOT bound to remainder fields
                                                 [U-REC]
```

Subsumptive fallback for concrete types (no type variables on either side):

```
unify(σ, τ, S) where ¬has_type_vars(σ) ∧ ¬has_type_vars(τ):
    if is_subtype(σ, τ) ∨ is_subtype(τ, σ): S   [U-SUBSUME]
    else: error                                  [U-FAIL]
```

[U-SUBSUME] is the bridge between unification and subtyping. It fires after all other rules (structural decomposition, type variable binding) have been tried. When two concrete types remain and they are in a subtype relationship in either direction, unification succeeds without modifying the substitution. This is essential for **confluence in CALL-POLY**: when a type variable α is bound to `IntLiteral(42)` by one argument and later compared against `Int` by another (via substitution resolution), [U-SUBSUME] recognizes `IntLiteral(42) <: Int` and succeeds regardless of argument order.

**Relationship to Robinson unification.** Robinson (1965) is purely syntactic — it has no notion of subtyping, so `unify(IntLiteral(42), Int)` would simply fail (different constructors). [U-SUBSUME] extends Robinson with a ground-type compatibility check: when both sides are concrete and in a subtype relationship, unification succeeds without modifying the substitution. This is a pragmatic middle ground — Robinson handles structural decomposition and variable binding; [U-SUBSUME] handles the subtype lattice at ground types. The substitution is never modified by [U-SUBSUME], so existing variable bindings (which may carry literal precision) are preserved. This is the same approach Rust's type inference uses: subtyping constraints between concrete types are resolved as compatibility checks rather than LUB computation (Dolan & Mycroft 2017 describe the full alternative — algebraic subtyping — which tinct intentionally does not adopt; see `doc/whatif/algebraic.md`).

[U-SUBSUME] checks both directions because unification is symmetric — the two types arrive without a designated "actual" vs "expected" role. The bidirectional check covers both orderings: `unify(IntLiteral(42), Int)` succeeds (IntLiteral(42) <: Int) and `unify(Int, IntLiteral(42))` also succeeds (IntLiteral(42) <: Int, checked as `is_subtype(τ, σ)`). The substitution is unchanged because there are no type variables to bind.

**Interaction with [SUB]:** At CALL-MONO sites (fully concrete types, no unification needed), `check_expr` uses directional subsumption via `is_subtype(actual, expected)` — only the correct direction is checked. [U-SUBSUME] is bidirectional because it operates within unification where the original directionality is lost after structural decomposition. This is sound because the substitution is not modified — the bidirectional check only determines compatibility, not binding direction.

All other non-structural, non-subsumable combinations: error [U-FAIL]

**Interaction with CALL-POLY:** Polymorphic call checking synthesizes all argument types, then unifies each against the corresponding instantiated parameter type. Type variable binding comes from [U-VAR]; concrete type compatibility (after substitution resolves variables) comes from [U-SUBSUME]. The bidirectional subsumption in [U-SUBSUME] ensures confluence — argument order does not affect whether type checking succeeds, only the precision of the resulting binding.

#### Subtyping: τ <: σ

Subtyping is a pure predicate (no substitution mutation). Used for TypeAssert validation and return type checking.

```
τ <: Any                                         [S-ANY-TOP]
Any <: τ                                         [S-ANY-BOT]
τ <: τ                                           [S-REFL]
IntLiteral(n) <: Int <: Number                   [S-INT]
StringLiteral(s) <: Str                          [S-STR]
Float <: Number                                  [S-FLOAT]
Seq(τ) <: Seq(σ)  if τ <: σ                      [S-SEQ]

Record(F₁,ρ₁) <: Record(F₂,ρ₂) if:
    ∀(k:σ) ∈ F₂, ∃(k:τ) ∈ F₁ with τ <: σ       (width+depth)
    If ρ₂ = Closed: keys(F₁) ⊆ keys(F₂)
        (combined with width condition, enforces exact key equality)
    If ρ₂ = Open | RowVar: always ok              [S-REC]

Fn(p₁...pₙ→r₁) <: Fn(q₁...qₙ→r₂) if:
    |p| = |q|
    qᵢ <: pᵢ  for all i                          (contravariant params)
    r₁ <: r₂                                      (covariant return)
                                                 [S-FN]
```

**Note on [S-ANY-TOP] and [S-ANY-BOT]:** Having Any as both the top and bottom of the type lattice violates antisymmetry (τ <: σ ∧ σ <: τ ⇒ τ = σ) and makes the subtype relation unsound as a partial order. This is intentional for tinct's gradual type system — Any marks the boundary between typed and untyped code (see Limitation #5).

#### Instantiation

```
instantiate(τ) = (S(τ), S)
    where S = {α₁ ↦ _t0, α₂ ↦ _t1, ...}
    for each αᵢ ∈ FTV(τ), fresh names _tN generated
    from a monotonic per-file counter.

FTV(τ) includes both type variables (α) and row variables
(RowVar(r)). Tinct conflates these into a single namespace —
both are collected by collect_type_vars() and renamed by
instantiate(). In Rémy (1994), row variables inhabit a
distinct kind from type variables; tinct does not enforce
this distinction.
```

This is alpha-renaming for call-site freshening. Each polymorphic call site gets independent type variables so unification at one site does not constrain another. With let-generalization (below), instantiation also handles let-bound polymorphic type schemes.

#### Let-Generalization (Levels-Based)

Tinct uses levels-based let-generalization following Kiselyov (2013) to support polymorphic let-bindings. This extends annotation-driven polymorphism with automatic generalization at dict entry boundaries.

**Type schemes.** The type environment Γ maps names to *type schemes* σ rather than bare types τ:

```
σ ::= ∀α₁...αₙ. τ    (n ≥ 0; when n = 0, equivalent to monomorphic τ)
```

Implementation: `TypeEnv.bindings` changes from `IndexMap<String, Type>` to `IndexMap<String, TypeScheme>`. The `TypeScheme` struct:

```rust
#[derive(Debug, Clone)]
pub struct TypeScheme {
    pub vars: Vec<String>,  // quantified variable names
    pub body: Type,
}

impl TypeScheme {
    pub fn mono(ty: Type) -> Self {
        Self { vars: vec![], body: ty }
    }
}
```

`PartialEq` for `TypeScheme` compares structurally (vars + body). `Display` shows `∀a b. Fn(a → b)` for polymorphic schemes, or the bare type for monomorphic ones. Located in `types.rs`.

**Levels.** Every type variable α carries an integer level ℓ(α). The type checker maintains a current level counter ℓ_current, incremented at each dict boundary (every `infer_dict` call):

- Fresh type variables are created at ℓ_current
- `Type::TypeVar(String)` becomes `Type::TypeVar(String, u32)` (name + level)
- `PartialEq` for `Type` is implemented manually: `TypeVar(a, _) == TypeVar(b, _)` compares names only, ignoring levels. This preserves the [U-REFL] fast path in `unify()`.
- `RowRest::RowVar(String)` becomes `RowRest::RowVar(String, u32)` — row variables carry levels and participate in generalization identically to type variables. (After the §Row-Variable Unification migration, `RowRest` becomes `RowTail`; levels carry over.)
- `Display` for `TypeVar` and `RowVar` hides the level (internal inference state, not user-facing).

**Level storage and mutation.** Levels must be mutable during unification (Kiselyov's level lowering). Since `Type` is a value type, levels are stored in a separate mutable map alongside the substitution:

```rust
pub struct InferState {
    pub name_counter: u32,   // monotonic fresh variable name counter
    pub level: u32,          // current binding depth
    pub levels: HashMap<String, u32>,  // var name → current level
}
```

When a `TypeVar(name, lvl)` is created, `levels[name] = lvl` is recorded. During unification, level lowering mutates `levels[name]` without rebuilding the `Type`. `generalize()` consults `levels` for the authoritative level of each variable. The level embedded in `TypeVar(String, u32)` is the *creation-time* level; `InferState.levels` is the *current* (possibly lowered) level.

**Level adjustment during unification (symmetric).** Both branches of type variable unification perform level lowering:

```
unify(α, τ, S) = S[α ↦ τ]
    if α ∉ FV(τ)                                   [occurs check]
    and set ℓ(β) = min(ℓ(β), ℓ(α))
        for all β ∈ FTV(τ)                         [U-VAR-LEVEL]

unify(τ, α, S) = S[α ↦ τ]
    if α ∉ FV(τ)                                   [occurs check]
    and set ℓ(β) = min(ℓ(β), ℓ(α))
        for all β ∈ FTV(τ)                         [U-VAR-LEVEL-SYM]
```

Both rules lower levels symmetrically: when binding α to τ, every type variable β inside τ has its level lowered to `min(ℓ(β), ℓ(α))`. This prevents variables from escaping their scope through either side of a unification.

**Any-unification and generalization.** When a type variable α is unified with `Any`, the current [U-ANY] rules succeed without binding α. To prevent incorrect generalization of the unbound α, `unify(α, Any)` sets `ℓ(α) = 0` (below all binding levels):

```
unify(α, Any, S) = S,  set ℓ(α) = 0               [U-ANY-VAR]
unify(Any, α, S) = S,  set ℓ(α) = 0               [U-VAR-ANY]
```

This ensures Any-touched variables are never generalized (since `ℓ(α) = 0` is never `> ℓ` for any binding level). The variable remains free and resolves to its eventual binding (if any) or stays unconstrained.

**Generalization.** At a dict boundary at level ℓ, after all entries in the letrec group are inferred:

```
generalize(ℓ, τ) = ∀{α | α ∈ FTV(τ), ℓ(α) > ℓ}. τ     [GEN]
```

where ℓ(α) is read from `InferState.levels[α]` (the current, possibly lowered level). Type variables whose level exceeds the enclosing scope's level are local to the binding and can be universally quantified. Variables at or below level ℓ are free in the enclosing scope and must remain monomorphic. Row variables participate identically — `RowVar(r, _)` with `levels[r] > ℓ` is generalized.

Implementation signature:

```rust
pub fn generalize(level: u32, ty: &Type, state: &InferState) -> TypeScheme
```

Collects FTV(ty) via a level-aware traversal returning `Vec<(String, u32)>` pairs, filters by `current_level > level`, returns `TypeScheme { vars, body: ty.clone() }`.

**Modified VAR rule:**

```
Γ(x) = ∀α₁...αₙ. τ
τ' = instantiate_scheme(∀α₁...αₙ. τ, ℓ_current)
────────────────────────────────── [VAR-POLY]
Γ ⊢ $x : τ'
```

Each variable reference instantiates its type scheme with fresh variables at ℓ_current. When n = 0, this returns the body directly (monomorphic binding — no allocation).

Implementation signature:

```rust
pub fn instantiate_scheme(
    scheme: &TypeScheme,
    level: u32,
    state: &mut InferState,
) -> Type
```

Creates fresh `TypeVar(_tN, level)` for each quantified variable, registers them in `state.levels`, applies the renaming substitution to the scheme body.

**Modified dict inference (letrec with generalization):**

```
Pass 0 — Key resolution: unchanged
Pass 1 — Bind all: Γ' = Γ, k₁:α₁, ..., kₙ:αₙ
         where each αᵢ is a fresh type variable at level ℓ+1.
         Forward references see αᵢ (participates in unification,
         unlike the previous Any which silently matched everything).
Pass 2 — Type aliases: unchanged. Aliases remain monomorphic
         (IndexMap<String, Type>, not TypeScheme).
Pass 3 — Infer values: at level ℓ+1, for each non-alias
         entry kᵢ, infer Γ' ⊢ eᵢ : τᵢ, then unify(αᵢ, τᵢ).
         Apply resulting substitution S.
Pass 4 — Generalize (NEW): for each entry kᵢ,
         σᵢ = generalize(ℓ, S(αᵢ), state)
         Update Γ'(kᵢ) = σᵢ
Build Record(k₁:S(α₁)...kₙ:S(αₙ), Closed).
────────────────────────────────── [DICT-GEN]
```

The Record type uses monomorphic (substitution-applied) types for the type map and downstream structural checks. The type schemes σᵢ live in Γ and are instantiated at each reference via [VAR-POLY].

**Nested dicts increment levels.** Each `infer_dict` call increments ℓ_current. For `[a: [b: 42]]`, the outer dict runs at ℓ+1 and the inner dict at ℓ+2. This matches standard HM let-nesting: each `let` increments the level.

**Forward references within letrec.** Within a single dict (letrec group), all entries share level ℓ+1 during Pass 3 inference. Forward references see the monomorphic αᵢ from Pass 1 — these are fresh type variables that participate in unification, producing binding constraints. This is more precise than the previous behavior (binding to `Any`): forward references now produce real type constraints rather than silently succeeding. After Pass 4, downstream consumers of the dict see polymorphic schemes.

Mutually recursive entries constrain each other through unification during Pass 3. This is standard monomorphic letrec (OCaml, Haskell `let rec`) — entries see each other as monomorphic during inference, not polymorphic. Polymorphic recursion (Mycroft 1984) is not supported: it would require fixpoint iteration to convergence, which is more complex and can diverge. The monomorphic restriction is sufficient for tinct's use cases.

**Document-level scheme threading.** `typecheck_document` splats dict Record fields into the parent environment for downstream document expressions. To preserve polymorphism across `---` boundaries, the splat must carry type schemes alongside the Record type. Implementation: `infer_dict` returns both the `Record` type (for structural checks) and an `IndexMap<String, TypeScheme>` (for environment threading). `typecheck_document` inserts the schemes into the parent `TypeEnv`.

**Interaction with `Any` and unannotated parameters:**

- Unannotated function parameters still receive type `Any` (not a fresh type variable). `[fn [x] $x]` remains `Fn(Any → Any)`.
- `Any` in unification acts as a universal match ([U-ANY-L], [U-ANY-R]) but sets ℓ(α) = 0 for any type variable α it touches ([U-ANY-VAR], [U-VAR-ANY]), preventing generalization.
- Annotated type variables (e.g., `x@a`) create fresh type variables at ℓ_current. These participate in generalization normally.
- The practical effect: let-generalization benefits code that uses type annotations. `[id: [fn [x@a] $x]]` generalizes `id` to `∀a. Fn(a → a)`; subsequent `[call $id 42]` and `[call $id "hello"]` each get independent instantiations.

**Interaction with CALL-POLY.** VAR-POLY instantiates type schemes at reference sites. For call expressions, the instantiated type typically has no remaining type variables (the fresh `_tN` variables are monomorphic instances), so CALL-POLY sees `has_type_vars() = false` and takes the CALL-MONO fast path. Double instantiation only occurs when a polymorphic function *returns* a polymorphic function — rare in practice. No optimization needed for the common case.

**Substitution name uniqueness.** `Substitution::map` is keyed by variable name. User-annotated type variables (e.g., `@a`) are not globally unique, but `instantiate_scheme()` renames them to fresh `_tN` names before any substitution sharing occurs. Within a single letrec group during Pass 3, each entry's annotation-derived variables are instantiated independently, preventing collision.

**Error recovery.** If Pass 3 inference fails for an entry, `Type::Any` is inserted for that entry (matching current behavior). Level lowering from partial unification before the failure is retained in `InferState.levels` — this is conservative (may prevent generalization of some variables) but safe. Generalization in Pass 4 proceeds for successfully-inferred entries; failed entries get `TypeScheme::mono(Type::Any)`.

**Key invariants:**

1. **Level monotonicity:** ℓ_current only increases when entering binding scopes. Fresh variables are always created at ℓ_current.
2. **Generalization soundness:** Only variables with ℓ(α) > ℓ_enclosing are generalized, ensuring no variable escapes its scope. Level lowering during unification ([U-VAR-LEVEL], symmetric) prevents variables from being captured at too high a level. Any-touched variables have ℓ = 0, preventing generalization.
3. **Value restriction (not needed):** Tinct does not have mutable references, so the value restriction (Wright, 1995) is unnecessary. All bindings can be generalized safely.
4. **Occurs check:** Unchanged — prevents infinite types regardless of levels.
5. **Substitution idempotence:** Unchanged — transitive chasing is orthogonal to levels.
6. **Letrec monomorphism during inference:** Within a letrec group, entries see each other as monomorphic during Pass 3 (fresh type variables, not schemes). Polymorphism only becomes visible after Pass 4 generalization.
7. **PartialEq level-blindness:** `TypeVar` equality ignores levels, preserving [U-REFL] semantics. Levels are consulted only during generalization (via `InferState.levels`).

**Implementation changes summary:**

| Component | Current | After |
|-----------|---------|-------|
| `Type::TypeVar` | `TypeVar(String)` | `TypeVar(String, u32)` — manual `PartialEq` (name only) |
| `RowRest::RowVar` | `RowVar(String)` | `RowVar(String, u32)` — levels for row generalization (becomes `RowTail::RowVar` after §Row-Variable Unification) |
| `TypeEnv.bindings` | `IndexMap<String, Type>` | `IndexMap<String, TypeScheme>` |
| `TypeEnv.type_aliases` | `IndexMap<String, Type>` | Unchanged — aliases stay monomorphic |
| `TypeEnv::get()` | Returns `&Type` | Returns `&TypeScheme` |
| `TypeEnv` | No `insert_scheme` | Add `insert_scheme(name, TypeScheme)` |
| `infer_expr` VAR case | `env.get(name).cloned()` | `instantiate_scheme(env.get(name)?, ...)` |
| `infer_dict` | 3 passes, bind to `Any` | 4 passes, bind to fresh αᵢ, generalize |
| `infer_dict` return | `Type` | `(Type, IndexMap<String, TypeScheme>)` |
| `typecheck_document` | Splats `Record` fields as `Type` | Splats `TypeScheme`s into parent env |
| `instantiate()` | `fn(Type, &mut u32) → (Type, Subst)` | Kept for CALL-POLY call-site freshening |
| New: `instantiate_scheme()` | — | `fn(TypeScheme, u32, &mut InferState) → Type` |
| New: `generalize()` | — | `fn(u32, Type, &InferState) → TypeScheme` |
| `unify()` U-VAR | Bind without level check | Bind + symmetric level lowering |
| `unify()` U-ANY + TypeVar | No binding, no level change | Set ℓ(α) = 0 to prevent generalization |
| `counter: Cell<u32>` | Name counter only | Replaced by `InferState` (name counter + level + level map) |
| `collect_type_vars()` | Returns `BTreeSet<String>` | Returns `BTreeSet<(String, u32)>` (name + level) |
| `Type::Display` | Shows `TypeVar` name | Shows name only (level hidden) |
| Tests: `TypeVar("a".into())` | — | All become `TypeVar("a".into(), 0)` |

**Future work:** Polymorphic builtin signatures (e.g., `map: ∀a b. Fn(Fn(a → b) × Seq(a) → Seq(b))`) become possible with type schemes. Currently all builtins are typed `Any`. Tracked as a type-extensions milestone.

**Principal types.** Tinct infers principal types for fully-annotated polymorphic functions where no type variable unifies with `Any`. For partially-typed code, the inferred type depends on the checking context — subsumption introduces multiple valid types for the same expression (e.g., `42` can check against `IntLiteral(42)`, `Int`, `Number`, or `Any`). Full Damas-Milner principality is not achieved because: (a) unannotated parameters receive `Any` rather than fresh type variables, (b) singleton literal types introduce subtyping which bidirectional checking mediates but which prevents a unique most-general type, and (c) [U-SUBSUME] in CALL-POLY means the type variable binding may be more or less precise depending on argument order (both bindings are sound, but they differ).

**References:** Kiselyov, O. (2013). "How OCaml type checker works — or what polymorphism and garbage collection have in common." Damas, L. & Milner, R. (1982). "Principal type-schemes for functional programs." Mycroft, A. (1984). "Polymorphic type schemes and recursive definitions." Wright, A. (1995). "Simple imperative polymorphism."

#### Limitations and Non-Guarantees

1. **Let-generalization not yet implemented.** The levels-based generalization design (see §Let-Generalization above) is specified but not yet implemented. Currently, functions without type variable annotations are monomorphic: `[fn [x] $x]` infers as `Fn(Any → Any)`, not `∀α. Fn(α → α)`. The principal type property of Damas-Milner (1982) does not hold until the design is implemented. Polymorphism currently requires explicit annotation: `[fn [x@a] $x]` gives `Fn(a → a)`, which is instantiated per call site.

2. **Literal promotion handled by bidirectional checking (not unification).** Literal-to-parent type compatibility (e.g., `IntLiteral(42) <: Int`) is handled exclusively by `is_subtype` in checking mode via the [SUB] rule — see §Bidirectional Typing. Unification is pure Robinson: `unify(IntLiteral(42), Int)` fails because these are distinct types. The previous bidirectional silent coercion rules have been removed. This preserves type precision and properly separates subtyping from unification (Pierce & Turner 2000).

3. **Row variables not bound in unification.** `unify(Record)` does not bind row variables to remainder fields. Row variable annotations parse and display but are not propagated through inference. Planned for the row-unification milestone.

4. **Named args not unified.** Named arguments in `[call ...]` are type-checked (values inferred) but not unified against function parameter types. Requires extending `Type::Function` to carry parameter names.

5. **Any is both top and bottom.** `Any <: τ` and `τ <: Any` for all τ. In subtyping theory, this makes `Any` simultaneously the top and bottom element of the type lattice, which is unsound in general. In tinct's advisory type system this is intentional — `Any` marks the boundary between typed and untyped code, and `[@Type expr]` is the explicit narrowing mechanism.

6. **Forward references are monomorphic within letrec.** In letrec dicts, entries that reference later siblings see a fresh type variable (from Pass 1), not the eventually-generalized type scheme. Within the letrec group, mutual references are monomorphic — each entry constrains the others through unification. Polymorphic recursion (Mycroft, 1984) would require fixpoint iteration and is not supported. (Note: prior to let-generalization, forward references resolved to `Any`.)

7. **Monomorphic call arguments now checked (resolved).** With bidirectional typing, CALL-MONO uses `check_expr(arg, param_type)` for each argument, catching type mismatches via subsumption. `[call $add "hello"]` where `$add : Fn(Int Int → Int)` now produces a type error because `String ≮: Int`. This resolves the former limitation where monomorphic calls skipped argument type checking.

8. **Variadic params typed as closed empty record.** Variadic parameters (`...args`) are assigned type `Record([], Closed)` but should be `Any`. Annotations on variadic params are forbidden by design: the runtime collects remaining positional args into an Int-keyed Dict, but row types only describe string-keyed records, so annotations cannot participate in type inference. This decision may be revisited when `Seq` types land, as variadic params could collect into a typed `Seq<T>` instead. Tracked in TODO.md.

#### TypeAssert Runtime Validation

**Not yet implemented — proposed design.**

The type checker and evaluator must agree on TypeAssert semantics. Currently they diverge: the static check is structural (`is_subtype(actual, expected)` in `resolve_type_assert`), while the runtime check is nominal (string comparison of `value.type_name()`). Record-type assertions like `[@[name: String age: Int] $expr]` pass type checking but are no-ops at runtime — the evaluator only sees "Dict" and cannot validate the record structure.

**Design: full structural convergence.** Both static and runtime TypeAssert checks are structural. The evaluator validates values against the full resolved `Type`, not just a type name string. Record fields are checked lazily via proxy contracts (Findler & Felleisen 2002), preserving tinct's lazy evaluation guarantees.

**Elaboration.** The type checker resolves TypeAssert annotations and embeds the resolved type directly in the AST (Dunfield & Krishnaswami 2021, §elaboration). This follows the standard bidirectional typing approach: the checking judgment produces an elaborated term where type information is explicit.

```
Expr::TypeAssert { expr, annotation }
→ Expr::TypeAssert { expr, annotation, resolved_type: Option<Type> }
```

The parser initializes `resolved_type: None`. The type checker fills it in during `resolve_type_assert()` via `resolve_annotation()`, applying the current substitution to produce a fully-substituted concrete type. Type aliases are resolved at this stage — the evaluator never resolves aliases itself. If type checking is skipped (`--no-typecheck` mode), `resolved_type` remains `None` and the evaluator degrades gracefully (see below).

Because the resolved type is part of the AST, it is captured naturally by `Unevaluated` thunks (which store `expr + env`). No changes to thunk state, `eval()` signatures, or environment structure are required.

**Structural validation judgment.** A judgment `v ∈ τ` ("value v inhabits type τ") defines structural validation at runtime. For primitive types, validation is immediate. For records, validation uses proxy contracts — field types are checked lazily when accessed, not eagerly at the assertion site.

*Immediate rules* (checked at assertion time):

```
────────────────────────────────── [VM-ANY]
v ∈ Any

v = Int(n),  n = m
────────────────────────────────── [VM-INT-LIT]
v ∈ IntLiteral(m)

v = Int(_)
────────────────────────────────── [VM-INT]
v ∈ Int

v = Int(_) ∨ v = Float(_)
────────────────────────────────── [VM-NUMBER]
v ∈ Number

v = Float(_)
────────────────────────────────── [VM-FLOAT]
v ∈ Float

v = String(s),  s = t
────────────────────────────────── [VM-STR-LIT]
v ∈ StringLiteral(t)

v = String(_)
────────────────────────────────── [VM-STR]
v ∈ Str

v = Bool(_)
────────────────────────────────── [VM-BOOL]
v ∈ Bool

v = Function{..} ∨ v = Builtin{..}
────────────────────────────────── [VM-FN]
v ∈ Fn(τ₁...τₙ → τᵣ)

v = Seq{..}
────────────────────────────────── [VM-SEQ]
v ∈ Seq(τ)

────────────────────────────────── [VM-VAR]
v ∈ α
```

*Proxy contract rule* (shape checked at assertion time, field types checked lazily):

```
v = Dict(entries),
∀(fᵢ: τᵢ) ∈ fields.  fᵢ ∈ string_keys(entries),
ρ = Closed ⟹ string_keys(entries) = dom(fields),
entries' = { fᵢ ↦ guard(entries[fᵢ], τᵢ, [fᵢ], span) | (fᵢ: τᵢ) ∈ fields }
          ∪ { k ↦ entries[k] | k ∉ dom(fields) }
────────────────────────────────────────────────────── [VM-RECORD-PROXY]
[@τ v] ⟶ Dict(entries')
```

Where `guard(thunk, τ, field_path, span)` creates a guarded thunk — a new `ThunkState::Guarded { inner, expected, field_path, guard_span }` variant in the thunk lifecycle. When materialized, a guarded thunk materializes the inner thunk, validates the result against τ via `v ∈ τ`, and either returns the value (on success) or raises a type assertion error with the field path (on failure). If τ is itself a `Record` type, validation applies [VM-RECORD-PROXY] recursively — the guarded thunk's materialized dict has its own fields wrapped in guards, composing field paths (e.g., `["user", "address", "zip"]`). Guards compose sequentially when nested TypeAsserts wrap the same value (Findler & Felleisen's "guardian stack" semantics).

**Guard memoization.** Guarded thunks follow standard thunk memoization: the type check executes once on first materialization, then the thunk transitions to `Materialized(validated_value)` (or `Failed(error)`). Subsequent accesses return the cached result without re-validation. This is the defunctionalized equivalent of Findler & Felleisen's `mon(τ, e)` contract monitor form.

If materialization of the inner thunk raises an error (e.g., division by zero), that error propagates immediately — it is not a type mismatch and does not trigger `default:`.

**Proxy contracts preserve laziness.** [VM-RECORD-PROXY] performs two phases: (1) *immediate shape validation* — required keys exist, cardinality for closed records — which is eager and runs at the assertion site, and (2) *lazy field type validation* — guard thunks that check field types on access. The key insight from Findler & Felleisen (2002): compound type contracts should defer field checking to the point of observation. A field that is never accessed is never validated — and never forced. This preserves the fundamental lazy evaluation guarantee: unreferenced values are never computed.

```lisp
data: [@[name: String age: Int] [call $from-json $input]]
# Shape check passes immediately (dict has "name" and "age" keys)
# $data.name — materializes, guard checks String, returns value
# $data.age — never accessed, never forced, never validated
```

**Validation depth by type constructor:**

| Type constructor | Validation | When | Rationale |
|-----------------|-----------|------|-----------|
| `Int`, `Float`, `Number`, `Str`, `Bool` | Exact | Immediate | Primitive — fully checkable |
| `IntLiteral(n)`, `StringLiteral(s)` | Exact | Immediate | Singleton value comparison |
| `Any` | Always passes | Immediate | Dynamic escape hatch |
| `Record(fields, Closed)` | Shape + cardinality | Immediate | Keys present, no extras |
| `Record(fields, Open)` | Shape only | Immediate | Required keys present |
| `Record` field types | Per-field via guard | On access | Proxy contract — lazy |
| `Fn(params → ret)` | Tag only — "is callable" | Immediate | Params/return opaque (Findler-style monitor would be needed for deep checking) |
| `Seq(τ)` | Tag only — "is sequence" | Immediate | Element type opaque; forcing all would diverge |
| `TypeVar(α)` | Always passes | Immediate | Residual from polymorphic instantiation; treated as `Any` |

Note on type-level variables: `TypeVar(α)` and `RowVar(r)` are both "variables" but serve different purposes. A `TypeVar` in a field type position indicates unconstrained polymorphism — treated as `Any` at runtime. A `RowVar` in the row rest position indicates structural openness — treated as `Open` at runtime (allow extra fields). `TypeVar` values in `resolved_type` arise only from polymorphic type schemes where a variable was not constrained during inference. Unresolved type aliases produce a `TypeError` during elaboration — they never reach the evaluator as `TypeVar`.

**Function and sequence types are opaque at runtime.** `[@[Fn@Int [String]] $f]` verifies that `$f` is callable but cannot verify parameter or return types without executing the function. `[@[Seq Int] $s]` verifies that `$s` is a sequence but cannot verify element types without consuming it (which may diverge for infinite sequences). Both degenerate to tag checks. Full higher-order contract monitoring (Findler & Felleisen 2002) — wrapping functions to check arguments on each call and return values on each return — is a possible future extension but not part of this design.

**Closed record cardinality.** `[@[name: String age: Int] $expr]` (no `...` rest) is a closed record check: the dict must have exactly the string-keyed fields `name` and `age`, no more, no less. Positional entries (`Key::Int`) are invisible to the Record type (see §Type-theoretic implication) and are excluded from the cardinality check. `[@[name: String ...] $expr]` is an open record check: requires `name: String` but allows additional fields. `RowVar(r)` is resolved by the type checker before elaboration; if unresolved (§Row-Variable Unification not yet implemented), it is treated as `Open`.

**Key type handling.** Record field names are strings, but `Value::Dict` entries use `Key::Int` for positional entries and `Key::String` for named entries. Field lookup during [VM-RECORD-PROXY] shape checking tries `Key::String(fᵢ)` first, then `Key::Int(fᵢ.parse())` as fallback, matching the type checker's Pass 0 key resolution which converts integer literals to strings via `to_string()`.

**Type alias resolution.** TypeAssert annotations may reference type aliases:

```lisp
Person: [type [name: String  age: Int]]
person: [@Person $data]
```

The type checker resolves `Person` → `Record([name: Str, age: Int], Closed)` during elaboration and stores the resolved type in `Expr::TypeAssert.resolved_type`. The evaluator reads it directly — no alias registry at runtime.

**Interaction with `default:`.** `default:` is triggered only by type assertion failures, not by computation errors:

- *Shape mismatch* (missing key, cardinality violation): immediate type assertion failure → use `default:` if present, else raise error.
- *Guard failure* (field value has wrong type, detected on access): type assertion error at field access site → use `default:` if present in the original annotation, else raise error.
- *Materialization error* (division by zero, cycle, depth limit during field access): propagates as an exception, bypasses `default:`. Computation failures are distinct from type mismatches (Findler & Felleisen 2002, §blame).

**Interaction with bidirectional checking.** The static type checker uses `check_expr(inner, resolved_type)` for TypeAssert, applying [SUB]: synthesize the inner expression's type, then check `is_subtype(actual, expected)`. The runtime `v ∈ τ` judgment is the dynamic counterpart — it validates the same structural relationships against concrete values.

**Consistency invariant** (for deeply checkable types):

```
If Γ ⊢ e ⇒ σ  and  σ <: τ  and  eval(e) = v  and  τ is deeply checkable,
then v ∈ τ.
```

A type τ is *deeply checkable* when all constituents are fully observable at runtime: primitives, singleton literals, records (recursively), and `Any`. The invariant holds because `is_subtype` is more restrictive than `v ∈ τ` for these types.

For *opaque* type constructors (`Fn`, `Seq`), the invariant degenerates to tag-level soundness: [VM-FN] and [VM-SEQ] perform only tag checks, so they accept values that `is_subtype` would reject (e.g., `Fn(Int→Int) ∈ Fn(String→String)` succeeds at runtime). The forward direction still holds: if `is_subtype(σ, τ)` passes statically, the tag check will certainly pass at runtime. But the converse does not — runtime tag success does not imply static subtyping.

**Error messages.** Runtime validation errors report the structural path to the mismatch:

```
type assertion failed: expected [name: String  age: Int],
  field "age": expected Int, got String
```

For guard failures (detected on field access), the error includes the field path. For nested records, paths compose: `field "user"."address"."zip": expected Int, got String`.

**`--no-typecheck` mode.** When type checking is skipped, `resolved_type` is `None`. The evaluator falls back to the current nominal behavior:

- Primitive type assertions (`Int`, `Float`, `String`, `Bool`, `Number`) still work — the annotation name is parsed directly and compared against `value.type_name()`. These are unaffected.
- Structural type assertions (`Record`, `Fn` with param types, `Seq` with element type) degrade to tag-only checks (`type_name() == "Dict"`, etc.) — no structural validation, no guard wrapping.

**Implementation changes summary:**

| Component | Current | After |
|-----------|---------|-------|
| `Expr::TypeAssert` | `{ expr, annotation }` | `{ expr, annotation, resolved_type: Option<Type> }` |
| Parser | — | Sets `resolved_type: None` |
| `resolve_type_assert()` | Returns resolved `Type` | Also sets `resolved_type` on the AST node |
| `eval()` TypeAssert branch | Extracts type name string, compares via `type_name()` | Reads `resolved_type`; primitives → `value_matches_type`; records → shape check + guard wrapping |
| `eval()` signature | Unchanged | Unchanged (no new parameters) |
| New: `value_matches_type()` | — | `fn(&Value, &Type, Span) -> Result<bool, EvalError>` — immediate rules only |
| New: `guard()` | — | `fn(Rc<Thunk>, Type, Vec<String>, Span) -> Rc<Thunk>` — wraps thunk in `Guarded` state |
| New: `ThunkState::Guarded` | — | `{ inner: Rc<Thunk>, expected: Type, field_path: Vec<String>, guard_span: Span }` |
| `type_name()` | Used for TypeAssert validation | Retained for error messages and `--no-typecheck` fallback |
| TypeAssert error messages | "expected Int, got String" | Structural path: "field \"age\": expected Int, got String" |
| `--no-typecheck` mode | Nominal check for all types | Nominal check for primitives, tag-only for structural types |

**References.** Findler, R. & Felleisen, M. (2002). "Contracts for Higher-Order Functions." Strickland, T.S., Tobin-Hochstadt, S., Findler, R. & Felleisen, M. (2012). "Chaperones and Impersonators: Run-time Support for Reasonable Interposition." Wadler, P. & Findler, R. (2009). "Well-Typed Programs Can't Be Blamed." Siek, J. & Taha, W. (2006). "Gradual Typing for Functional Languages." Dunfield, J. & Krishnaswami, N. (2021). "Bidirectional Typing."

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
- `$filter` returns a Seq of matching values (since inclusion requires predicate evaluation, keys are not preserved) — use `$collect` to get a dict back
- The type system enforces the boundary: list operations require `[a]` (integer-keyed)

```lisp
# $filter returns a Seq of matching values (dual-dispatch)
$data: [alice bob carol dave]
[call $filter [fn [x] [call $not [call $= $x bob]]] $data]
# → Seq(alice, carol, dave)    use $collect for a dict

# Pipe through $collect for a clean list
[call $collect [call $filter [fn [x] [call $not [call $= $x bob]]] $data]]
# → [0: alice  1: carol  2: dave]

# $filter on string-keyed dicts also returns Seq of values
[call $collect [call $filter [fn [v] [call $> $v 0]] [x: 1  y: -2  z: 3]]]
# → [0: 1  1: 3]
```

**`$conj` on sparse data:** `$conj` delegates to `$append`, which uses the maximum existing integer key + 1 as the new key (or 0 if no integer keys exist). This avoids key collisions even on sparse data:

```lisp
# Dense list — $conj works as expected
[call $conj [a b c] d]                  # → [0: a  1: b  2: c  3: d]

# Sparse data — no collision, key 11 is used (max 10 + 1)
$sparse: [0: a  5: b  10: c]
[call $conj $sparse d]                  # → [0: a  5: b  10: c  11: d]
```

#### Access Chain Evaluation — Formal Specification

Formalizes the three access forms (dot, bracket, range) as an access algebra with compositional chain semantics. Access chains are the primary data extraction mechanism in tinct — they desugar to nested AST nodes that the evaluator reduces inside-out, forcing the target at each step.

##### Part 1: Access Algebra

An **access chain** is a sequence of projections applied left-to-right to a target expression. The parser produces nested AST nodes; the algebra makes the compositional structure explicit.

**Projections.** A projection `π` extracts data from a dict:

```
π ::= dot(f)              — field access by literal string key f
    | bracket(e)          — field access by evaluated expression e
    | range(s?, e?)       — key-range slice with optional bounds [s, e)
```

**Chains.** An access chain `C = π₁ · π₂ · ... · πₙ` applied to target expression `t` evaluates as left-to-right composition:

```
eval_chain(t, [], ρ, d) = eval(t, ρ, d)                          (empty chain)
eval_chain(t, [π₁, ...πₙ], ρ, d) = eval_chain(apply(π₁, t, ρ, d), [π₂, ...πₙ], ρ, d)
```

**Parser correspondence:** The parser produces nested AST nodes for chains. `$a.b[0].c` parses as:

```
DotAccess(
  BracketAccess(
    DotAccess(VarRef("a"), "b"),
    Int(0)),
  "c")
```

The evaluator reduces inside-out: first `eval(VarRef("a"))`, then `apply(dot("b"), ...)`, then `apply(bracket(0), ...)`, then `apply(dot("c"), ...)`. This inside-out reduction is equivalent to the left-to-right chain evaluation defined above.

##### Part 2: Projection Rules

Each projection forces its target to a `Dict`, then extracts by key. All three rules share a common forcing step formalized as `force_dict`.

**[FORCE-DICT]** — Common target forcing

```
θ_target = eval(target, ρ, d+1)
v = force(θ_target, d+1)                    (inherent materialization — must know dict structure)
v = Dict(map)                               (target must be Dict; type error otherwise)
────────────────────────────────────────────
force_dict(target, ρ, d) ⇒ map
```

If `v` is not a `Dict`, evaluation fails with `type_mismatch("Dict", v.type_name(), span)`. This is inherent materialization (§Selective Materialization) — the dict structure must be known to perform key lookup. FORCE-DICT is a composite rule combining `eval`, `force`, and pattern match — it is not a primitive judgment of the Thunk Lifecycle. All three projection rules below conclude with `⇒ Rc<Thunk>` — ACCESS-DOT and ACCESS-BRACKET return an alias to an existing thunk in the dict, while ACCESS-RANGE wraps its result in a fresh `Materialized` thunk.

**[ACCESS-DOT]** — Dot access: `$target.field`

```
map = force_dict(target, ρ, d)
key = String(field)                          (field is a literal string from the AST)
map[key] = θ                                 (look up key; error if absent)
────────────────────────────────────────────
eval_dot(target, field, ρ, d) ⇒ θ
```

Error case: if `key ∉ dom(map)`, error `key_not_found(field, span)`. No default — missing keys are always errors (§No Null — Missing Keys Are Errors).

**[ACCESS-BRACKET]** — Bracket access: `$target[key_expr]`

```
map = force_dict(target, ρ, d)
key = eval_key(key_expr, ρ, d)              (evaluate key expression to String or Int)
map[key] = θ                                 (look up key; error if absent)
────────────────────────────────────────────
eval_bracket(target, key_expr, ρ, d) ⇒ θ
```

`eval_key` evaluates the key expression and materializes it to obtain a concrete `String` or `Int` key. This is the same `eval_key` used by DICT-SCOPE (§Scope Chain Semantics) — key evaluation is shared infrastructure.

Error case: if `key ∉ dom(map)`, error `key_not_found(key, span)`.

**[ACCESS-RANGE]** — Range access: `$target[start..end]`

```
map = force_dict(target, ρ, d)
s = start.map(|e| eval_key(e, ρ, d))        (optional start bound, evaluated)
e = end.map(|e| eval_key(e, ρ, d))          (optional end bound, evaluated)

result = {}
∀(k, θ) ∈ map (in insertion order):
  key_in_range(k, s, e) ⟹ result[k] ← θ   (include matching entries)
────────────────────────────────────────────
eval_range(target, start, end, ρ, d) ⇒ Materialized(Dict(result))
```

**Range semantics:** Half-open interval `[start, end)` — start inclusive, end exclusive. When `start` is `None`, all keys from the beginning are included. When `end` is `None`, all keys to the end are included. When both are `None` (`$data[..]`), all entries are included (identity slice).

**`key_in_range` comparability:**

```
key_in_range(k, s, e):
  ∀bound ∈ {s, e} where bound ≠ None:
    k.partial_cmp(bound) must be Some(_)     (keys must be comparable)
  after_start = s = None ∨ k ≥ s
  before_end  = e = None ∨ k < e
  return after_start ∧ before_end
```

`Key::PartialOrd` returns `Some` for same-type comparisons (`Int-Int`, `String-String`) and `None` for mixed types (`Int-String`). When `partial_cmp` returns `None`, evaluation fails with `"range access requires comparable key types"`. Both bounds are checked unconditionally — a key that fails one bound may still error on the other if types are incomparable. In practice, this is unreachable because the type system requires homogeneous key types for range-accessed dicts (§Type Inference Algorithm).

**Result construction:** ACCESS-RANGE returns a `Materialized(Dict(result))` — unlike ACCESS-DOT and ACCESS-BRACKET which return an existing thunk from the dict, ACCESS-RANGE constructs a new dict. The individual entry thunks `θ` are shared (`Rc::clone`) with the source dict, preserving memoization. The `key_in_range` predicate determines the result *set* independently of iteration order (it tests each key against the bounds). Insertion order from the source dict is preserved in the result dict, affecting only the ordering of entries, not which entries are included.

##### Part 3: Error Taxonomy

Four error classes, each mapping to a specific point in the projection rules:

| Error | Rule | Condition | Message |
|-------|------|-----------|---------|
| Target not a Dict | FORCE-DICT | `v` is not `Dict` | `type_mismatch("Dict", v.type_name())` |
| Key not found (dot) | ACCESS-DOT | `String(field) ∉ dom(map)` | `key_not_found(field)` |
| Key not found (bracket) | ACCESS-BRACKET | `key ∉ dom(map)` | `key_not_found(key)` |
| Incomparable keys (range) | ACCESS-RANGE | `partial_cmp` returns `None` | `"range access requires comparable key types"` |

Error context is enriched via `push_frame`: dot access adds `"accessing .{field}"`, bracket adds `"accessing [..]"`, range adds `"accessing [..:..]"`. This stack frame identifies which step in a chain failed.

##### Part 4: Chain Properties

Five properties that hold for all access chains.

**Property 1: Step-wise Forcing**

*Statement:* Each projection in a chain invokes FORCE-DICT exactly once. In a chain `π₁ · π₂ · ... · πₙ`, FORCE-DICT is invoked `n` times — once per step. FORCE-DICT evaluates and forces the target — if the target thunk is already `Materialized`, forcing is a cache hit (FORCE-CACHED from §Thunk Lifecycle).

*Proof sketch:* By induction on chain length. Each `apply(πᵢ, ...)` invokes FORCE-DICT, which calls `force(θ, d+1)`. The result of step `i` becomes the target of step `i+1`. No step forces the target of a different step. ∎

**Property 2: Result Laziness**

*Statement:* ACCESS-DOT and ACCESS-BRACKET return the thunk stored in the dict without forcing it. The result may be `Unevaluated`, `PendingBuiltin`, `PendingCall`, or `Materialized` — access does not trigger evaluation of the accessed value.

*Proof sketch:* Both rules return `Rc::clone(thunk)` from `map.get(&key)` — a pointer copy, not a `force` call. The thunk's state is unchanged by the access. ACCESS-RANGE also preserves laziness of individual entries (shared via `Rc::clone`), though it constructs a new `Materialized(Dict(...))` wrapper. ∎

**Property 3: Error Short-Circuiting**

*Statement:* If projection `πᵢ` in a chain fails, projections `πᵢ₊₁, ..., πₙ` are never evaluated.

*Proof sketch:* By the chain recurrence, `eval_chain(t, [π₁, ...πₙ], ρ, d)` first computes `apply(π₁, t, ρ, d)`. If this returns an error, the recurrence has no value to pass to the next step, so the chain terminates with that error. By induction, no subsequent projection is evaluated. ∎

**Property 4: Depth Consumption**

*Statement:* A chain of length `n` consumes `n` depth levels — each FORCE-DICT invocation increments depth by 1 (via `eval(target, ρ, d+1)` and `force(θ, d+1)` in `eval_as_dict`).

*Proof sketch:* By inspection of FORCE-DICT, which passes `d+1` to both `eval` and `force`. Each chain step invokes FORCE-DICT once (Property 1), so `n` steps consume `n` depth levels. For `MAX_EVAL_DEPTH = 256` and typical chain lengths (1–5), this is negligible. After the CEK migration removes MAX_EVAL_DEPTH, this property becomes moot. ∎

**Property 5: Sharing Preservation**

*Statement:* ACCESS-DOT and ACCESS-BRACKET return an `Rc::clone` of the thunk stored in the dict — an alias, not a copy. If the same field is accessed twice, both accesses obtain pointers to the same `Rc<Thunk>`. Once the first access forces it, the second access gets FORCE-CACHED (§Thunk Lifecycle). ACCESS-RANGE creates a new dict wrapper but shares entry thunks via `Rc::clone`, so memoization is preserved for individual entries.

*Proof sketch:* ACCESS-DOT and ACCESS-BRACKET return `Rc::clone(thunk)` from `map.get(&key)`. The `Rc` reference count increases, but both the dict entry and the accessor hold pointers to the same `Thunk`. When either forces it, the thunk transitions to `Materialized` (or `Failed`), and subsequent accesses via any alias see the cached state. This is the Launchbury (1993) sharing guarantee applied to record projection — access is observation, not duplication. ∎

##### Part 5: Type System Correspondence

**Current limitation:** Access chain type checking is direct structural lookup, not constraint generation. This is a consequence of the incomplete row-variable unification implementation, not a design choice. The target type is inferred first, then the result type is determined by structural matching on the inferred target type. No type variables are introduced or bound by access operations, and access type checking is read-only with respect to the unification substitution — the target type is normalized via `apply_subst` before field lookup, but no new bindings are added. This differs from constraint-based systems (e.g., Elm, OCaml) where `$x.field` would generate a constraint `unify(typeof(x), Record([field: α], ρ))` and bind `α` and `ρ`. When full Rémy-style row-variable unification is implemented (§row-unification), access chains should generate such constraints, enabling the type checker to infer field requirements from usage without annotations.

The type checker mirrors the access algebra with type-level projections:

| Runtime rule | Type rule | Type-level behavior |
|-------------|-----------|-------------------|
| ACCESS-DOT | `check_dot_access` | `Record(fields) → fields[f]`; open record → `Any`; closed + missing → error |
| ACCESS-BRACKET | `check_bracket_access` | Literal key → exact field lookup; variable key → `Any`; open record → `Any` |
| ACCESS-RANGE | `check_range_access` | Bounds must be `Int` or `Str`; result type = target type (preserves record type) |

**Type variable access:** Accessing a field on a type variable (`TypeVar(α)`) is a type error in the current implementation (`typecheck.rs:313` falls through to `not_a_record`). LLT does not perform constraint-based row unification that would bind `α` to `Record([field: β], ρ)`. When full row-variable unification is implemented (§Row Polymorphism, planned), this behavior may change. Row variables (`RowVar(r)`) appearing in record types are treated as markers for openness during access type checking; they are not bound to remainder types during access operations (consistent with U-REC in §Type Inference Algorithm).

**Open records and Any:** When a dot or bracket access targets an open record (`Record(fields, Open)` or `Record(fields, RowVar(_))`) and the field is not in `fields`, the type checker returns `Any` rather than an error. This reflects LLT's gradual typing design: open records may contain fields not visible to the type checker. Rather than reject valid programs, the type checker admits the access but types the result as `Any`, deferring validation to runtime. This is sound because `Any` serves as both top and bottom type (S-ANY-TOP, S-ANY-BOT in §Type Inference Algorithm) — values of any type flow through `Any` positions. For closed records, a missing field is a static error.

**Bracket key precision:** When the bracket key is a literal (`Expr::Str` or `Expr::Int`) or has a singleton type (`StringLiteral(s)` or `IntLiteral(n)`), the type checker performs exact field lookup. When the key is a variable with type `Str`, `Int`, or `Any`, the result type is `Any` — since the key value is not known until runtime, the type checker cannot determine which field will be accessed, so it conservatively returns `Any`. This is the trade-off between expressiveness (allow computed keys) and precision (lose static type information).

**Range type preservation:** Range access conservatively types the result as the target type rather than attempting to narrow the field set (`typecheck.rs:384` returns `target_ty` unchanged). This is sound: the result dict is structurally a subtype of the target type (it contains a subset of the fields). Precise inference would require dependent types or refinement types to track which fields are included based on the runtime range bounds. The type checker does not currently verify that range bounds have compatible types with each other or with the target record's key types — `$data["a"..3]` with a String start and Int end passes type checking but fails at runtime. This is a known completeness gap; statically rejecting mixed-type bounds would require unifying the bound types.

##### Part 6: Implementation Correspondence

| Formal rule | Implementation | Source |
|------------|----------------|--------|
| FORCE-DICT | `eval_as_dict()` | `eval.rs:714-727` |
| ACCESS-DOT | `eval_dot_access()` | `eval.rs:729-746` |
| ACCESS-BRACKET | `eval_bracket_access()` | `eval.rs:748-765` |
| ACCESS-RANGE | `eval_range_access()` | `eval.rs:767-796` |
| `key_in_range` | `key_in_range()` | `eval.rs:26-46` |
| `Key::PartialOrd` | `impl PartialOrd for Key` | `value.rs:34-42` |
| Chain nesting | Parser produces nested `DotAccess`/`BracketAccess`/`RangeAccess` AST nodes | `ast.rs:79-93` |
| Type-level dot | `check_dot_access()` | `typecheck.rs:297-315` |
| Type-level bracket | `check_bracket_access()` | `typecheck.rs:317-357` |
| Type-level range | `check_range_access()` | `typecheck.rs:359-387` |

##### Part 7: Worked Examples

**Example 1: Chained dot access**

```lisp
[config: [database: [host: localhost  port: 5432]]]

[call $str $config.database.host]
```

Chain: `dot("database") · dot("host")` applied to `$config`.
1. `eval(VarRef("config"), ρ)` → `θ_config`
2. `force_dict(θ_config)` → `{database: θ_db}`. `map[String("database")]` → `θ_db`. Result: `θ_db` (lazy).
3. `force_dict(θ_db)` → `{host: θ_host, port: θ_port}`. `map[String("host")]` → `θ_host`. Result: `θ_host` (lazy).
4. `$str` forces `θ_host` → `"localhost"`.

Note: `θ_port` is never forced — Property 2 (result laziness) means accessing `.host` does not evaluate `.port`.

**Example 2: Mixed chain with bracket**

```lisp
$services[0].host
```

Chain: `bracket(Int(0)) · dot("host")`.
1. `force_dict($services)` → map. `eval_key(Int(0))` → `Key::Int(0)`. `map[Int(0)]` → `θ_svc0`.
2. `force_dict(θ_svc0)` → `{host: θ_host, ...}`. `map[String("host")]` → `θ_host`.

**Example 3: Range access**

```lisp
$data: [a: 1  b: 2  c: 3  d: 4]
$data[b..d]
```

`force_dict($data)` → `{a: θ₁, b: θ₂, c: θ₃, d: θ₄}`. Bounds: `s = String("b")`, `e = String("d")`.
- `key_in_range(String("a"), "b", "d")`: `"a" < "b"` → `after_start` = false → exclude.
- `key_in_range(String("b"), "b", "d")`: `"b" ≥ "b"` ∧ `"b" < "d"` → include.
- `key_in_range(String("c"), "b", "d")`: `"c" ≥ "b"` ∧ `"c" < "d"` → include.
- `key_in_range(String("d"), "b", "d")`: `"d" ≥ "b"` ∧ `"d" < "d"` → `before_end` = false → exclude.
- Result: `Materialized(Dict({b: θ₂, c: θ₃}))`. Half-open: start inclusive, end exclusive.

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

**`[]` is the only bracket type.** No `()` in the language. Every expression uses `[]`. The `call` keyword distinguishes function application from data — the bracket type is not needed for this. Positional and named entries may be freely interleaved — auto-indices are assigned sequentially to positional entries regardless of where named entries appear. See Principle 2 for the auto-indexing and parsing rules.

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
[x: 1 y]                       # OK — x→1 is named; y is auto-indexed as 0
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

**Why:** One-token-per-value eliminates ambiguity about where one entry ends and the next begins. The parser never has to guess whether a bare word belongs to the previous entry's value or starts a new entry. Every token's role is unambiguous from left to right.

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

#### `$_` Desugaring — Formal Specification

`$_` desugaring is a **pre-typecheck source-to-source AST transformation**. It runs after parsing and before both type checking and evaluation. The type checker and evaluator both see the desugared form (Scala, Clojure, and Elixir all desugar placeholder syntax before evaluation — none gate on the runtime environment). See Pombrio & Krishnamurthi (2014) for the formal framework motivating pre-evaluation desugaring; Krishnamurthi (2012, PLAI) for the standard pipeline ordering.

**Pipeline placement:**

```
source → parse → desugar_underscores → typecheck → eval
```

The pass operates on `Spanned<File>` (multi-document) and `Spanned<Expr>` (single expression for REPL). Both `eval_source()` and REPL entry points call the desugar pass after parsing.

**DIRECT predicate.** Tests whether an expression is `$_` or an access chain rooted at `$_`. Operates on **raw** (pre-desugaring) AST nodes. Access chain keys, range bounds, and dict entry keys are excluded — only the access *target* triggers desugaring:

```
DIRECT(e) = match e with:
  | VarRef("_")              → true
  | DotAccess(e', _)         → DIRECT(e')
  | BracketAccess(e', _)     → DIRECT(e')    -- target only, not key
  | RangeAccess(e', _, _)    → DIRECT(e')    -- target only, not bounds
  | _                        → false
```

**Rewrite rules.** The pass checks WRAP conditions on **raw** (un-desugared) children *before* recursing. DIRECT subtrees are left as-is inside the generated `Fn` body — they are variable references to the `_` parameter, not candidates for further wrapping. Non-DIRECT children are recursed into at depth+1 (inside the generated lambda, `_` is bound). This avoids the greedy-wrapping problem where naive bottom-up traversal would wrap `$_.age` before its enclosing Call could claim it (Visser 1998).

```
DESUGAR(e, depth) =
  -- Fn with _ param: increase depth, recurse into body only
  | Fn(params, body) where "_" ∈ params
      → Fn(params, DESUGAR(body, depth + 1))

  -- At depth > 0, _ is bound — recurse children, never wrap
  | _ where depth > 0
      → RECURSE_CHILDREN(e, depth)

  -- WRAP-CALL: check DIRECT on raw children, then wrap
  | Call(f, args, named)
      where not DIRECT(f)                               -- func position excluded
        and (∃ a ∈ args. DIRECT(a)
             or ∃ n ∈ named. DIRECT(n.value))
      → Fn([_], Call(                                    -- [WRAP-CALL]
            DESUGAR(f, depth + 1),                       -- recurse func
            [if DIRECT(a) then a                         -- keep DIRECT args as-is
             else DESUGAR(a, depth + 1) | a ∈ args],    -- recurse non-DIRECT args
            [if DIRECT(n.value) then n                   -- keep DIRECT named vals
             else n{value=DESUGAR(n.value, depth + 1)}   -- recurse non-DIRECT named vals
             | n ∈ named]))

  -- WRAP-DICT: same pattern — check raw, wrap, recurse non-DIRECT
  | Dict(entries)
      where ∃ entry ∈ entries. DIRECT(entry.value)
      → Fn([_], Dict(                                   -- [WRAP-DICT]
            [if DIRECT(e.value) then e
             else e{value=DESUGAR(e.value, depth + 1)}
             | e ∈ entries]))

  -- WRAP-DOT/BRACKET/RANGE: standalone access chain rooted at $_
  -- Only fires when no enclosing Call/Dict claimed it
  | DotAccess(target, field)
      where DIRECT(target)
      → Fn([_], DotAccess(target, field))                -- [WRAP-DOT]

  | BracketAccess(target, key)
      where DIRECT(target)
      → Fn([_], BracketAccess(                           -- [WRAP-BRACKET]
            target, DESUGAR(key, depth + 1)))

  | RangeAccess(target, lo, hi)
      where DIRECT(target)
      → Fn([_], RangeAccess(                             -- [WRAP-RANGE]
            target, DESUGAR(lo, depth + 1),
            DESUGAR(hi, depth + 1)))

  -- PASS: no wrapping, recurse into all children
  | _ → RECURSE_CHILDREN(e, depth)                       -- [PASS]
```

**Exclusions.** The following positions do **not** trigger desugaring:

- **Func position in Call:** `$_` or `$_.method` as the function in `[call $_ ...]` or `[call $_.method ...]` is an ordinary variable/access lookup. The WRAP-CALL rule requires `not DIRECT(f)`.
- **Bracket access keys:** `$data[$_]` — `$_` in the key position is not checked by DIRECT on the target.
- **Range bounds:** `$data[$_..5]` — bounds are not checked by DIRECT on the target.
- **Dict entry keys:** `[$_: value]` — WRAP-DICT checks `DIRECT(entry.value)` only, never `entry.key`.
- **TypeAssert values:** `[@Number $_.age]` — TypeAssert is not a WRAP form. The inner `$_.age` triggers WRAP-DOT independently, producing `[@Number [fn [_] $_.age]]` (a type assertion on a function). This is likely a user error; the type checker will report a mismatch.

**Boundary forms and scoping.** `Dict`, `Call`, and `Fn` are **lambda boundaries**. The WRAP rules check raw children before recursing, so each `$_` binds to the innermost enclosing bracket that triggers a WRAP rule:

```
[call $filter [call $> $_.age 30] $users]

Traversal (top-down check, selective recursion):
  1. Outer Call: DIRECT($users)? No. DIRECT([call $> $_.age 30])? No (Call is
     not DIRECT). No WRAP. RECURSE_CHILDREN.
  2. Inner Call: DIRECT($_.age)? Yes (in args). WRAP-CALL fires.
     → Fn([_], [call $> $_.age 30])
  3. Outer Call now has args = [<fn>, $users] — neither is DIRECT. Unchanged.
  Result: [call $filter [fn [_] [call $> $_.age 30]] $users]  ✓
```

**Shadowing.** If `_` is a parameter of an enclosing `Fn`, inner `$_` references refer to that parameter — they are ordinary variable references, not desugaring triggers. The `depth` parameter tracks this lexically:

- `depth = 0`: `$_` is unbound, WRAP rules apply.
- `depth > 0`: `$_` is bound by an enclosing `Fn([_] ...)`, RECURSE_CHILDREN only.

This replaces the current eval-time `env.borrow().get("_").is_none()` check with a purely syntactic scope analysis. The lexical approach is more precise: desugaring depends only on AST structure, never on the runtime environment.

**Invariants:**

1. **Syntactic determinism.** The desugaring result depends only on the AST structure, never on the runtime environment. The same expression always desugars the same way.
2. **Idempotence.** Applying `DESUGAR` to an already-desugared AST produces no changes (the generated `Fn` nodes have `_` as a single parameter, setting depth > 0 for inner references).
3. **Type visibility.** After desugaring, the type checker sees `Fn` nodes and can infer function types for `$_` expressions. With the current type checker (unannotated params default to `Type::Any`), `[call $add $_ 1]` types as `Fn(Any → Number)`. With future bidirectional checking, the call-site context could refine the parameter type — e.g., `[call $map $_.name $users]` where `$users: Seq[[name: Str ...]]` could check the lambda against `Fn([name: Str ...] → Str)`. Row-polymorphic parameter inference (see row-unification section) would further improve this to `Fn([name: α ...ρ] → α)`.

**Span preservation.** Generated `Fn` nodes reuse the span of the original expression. Error messages reference user-written syntax (`[call $add $_ 1]`), not the desugared form (`[fn [_] [call $add $_ 1]]`).

**Implementation sketch:**

```rust
fn desugar_file(file: Spanned<File>) -> Spanned<File> { /* walk documents/expressions */ }
fn desugar_expr(expr: Spanned<Expr>) -> Spanned<Expr> { desugar(expr, 0) }

fn desugar(expr: Spanned<Expr>, depth: usize) -> Spanned<Expr> {
    // Check WRAP conditions on raw children BEFORE recursing
    if depth == 0 {
        if let Some(wrapped) = try_wrap(&expr, depth) {
            return wrapped;
        }
    }
    // At depth > 0 or no WRAP match: recurse into children
    recurse_children(expr, depth)
}
```

**Migration from eval-time desugaring.** The current implementation in `eval()` (`should_desugar_underscore` + `wrap_in_lambda` at `src/eval.rs:66-71`) is removed once the AST pass is active. The pass subsumes it entirely. The eval-time functions (`contains_direct_underscore`, `call_has_direct_underscore`, `should_desugar_underscore`, `wrap_in_lambda`) move to a new `src/desugar.rs` module with the scope-tracking addition. Existing unit tests (`test_underscore_*` in `eval.rs`) must call `desugar_expr()` before `eval()`. The migration resolves TODO.md:44 ("$_ desugaring AST shape mismatch between type checker and evaluator").

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

#### Scope Chain Semantics — Formal Specification

Formalizes the two scoping mechanisms described above (letrec within dicts, sequential let* between expressions) using Launchbury's (1993) natural semantics for lazy evaluation, extended with Nakata & Hasegawa's (2009) cyclic call-by-need treatment for letrec cycle detection. The key insight is that both mechanisms are instances of the same primitive: `Environment::with_parent` creating a child scope linked to a parent chain.

##### Part 1: Domains and Notation

**Environments.** An environment `ρ` is a pair `(B, parent)` where `B : String → Thunk` is a finite map from names to thunks and `parent : Option<Env>` is a link to an enclosing scope. The parent chain forms a tree rooted at the builtins scope `ρ_builtins` (Property 4). The capture graph — thunks closing over their containing environment — may contain cycles in letrec scopes; see Property 4 for the distinction.

```
ρ ::= (B, None)            — root environment (builtins)
    | (B, Some(ρ_parent))  — child environment
```

**Thunks.** Thunks follow the lifecycle specified in §Thunk Lifecycle — Formal Specification. For scoping purposes, the relevant states are `Unevaluated(expr, ρ_capture)` (closes over an environment) and `Materialized(v)` (holds a value). The `ρ_capture` in an unevaluated thunk is the environment in which the expression will be evaluated — this is how letrec mutual visibility works: all entries in a dict capture the same shared `ρ_dict`.

**Keys.** Dict entries have keys `k ∈ Key = String(s) | Int(n)`. Only `String` keys produce scope bindings; `Int` keys are positional and do not enter any environment.

**Document pipeline variable.** The variable written `$$` in tinct source code appears as `$` in formal notation — `$` is the actual identifier name per the lexer rules. `$$` is syntactic sugar for `VarRef("$")`.

**Notation conventions.** `ρ(x)` denotes lookup of name `x` in environment `ρ` (defined formally in Part 3). `ρ[x ↦ θ]` denotes extending `ρ`'s bindings with `x` bound to thunk `θ`. `dom(ρ)` is the set of names bound directly in `ρ` (not including parent bindings). `eval(e, ρ, d)` is the evaluation judgment from §Thunk Lifecycle. The rules below use an implementation-oriented notation mixing imperative state updates (`ρ.B[s] ← θ`) with declarative judgments, following the same convention as §Thunk Lifecycle — Formal Specification Part 2.

##### Part 2: Environment Construction Rules

Two rules construct environments: DICT-SCOPE for letrec within a dict, and SEQ-SCOPE for sequential expressions in a document.

**[DICT-SCOPE]** — Letrec environment for dict literals

```
entries = [(k₁, e₁), ..., (kₙ, eₙ)]       (dict entries, keys + value exprs)
ρ_dict = ({}, Some(ρ_parent))               (fresh child env linked to parent)

∀i ∈ 1..n:
  kᵢ = eval_key(key_exprᵢ, ρ_parent, d)    (keys evaluated in PARENT scope)
  θᵢ = Unevaluated(eᵢ, ρ_dict)             (values close over SHARED dict env)
  kᵢ = String(sᵢ) ⟹ ρ_dict.B[sᵢ] ← θᵢ    (string keys become bindings)
  kᵢ = Int(_)     ⟹ no binding             (int keys are positional only)

∀i ≠ j: kᵢ ≠ kⱼ                             (duplicate keys are errors)
────────────────────────────────────────────
eval_dict(entries, ρ_parent, d) ⇒ Dict([(k₁,θ₁), ..., (kₙ,θₙ)])
```

When `entries = []` (empty dict), the quantifications over `i ∈ 1..n` are vacuous, and the rule produces `Dict([])` with `ρ_dict` containing no bindings.

The `∀i` is processed sequentially (source order). Bindings are inserted incrementally, so entry `i+1`'s thunk is created after entry `i`'s binding exists in `ρ_dict`. However, because no thunk is forced during construction (all remain `Unevaluated` — see construction-time non-forcing invariant below), the final state of `ρ_dict` is independent of insertion order, and the sequential semantics is observationally equivalent to simultaneous binding.

**Construction-time non-forcing invariant:** No thunk in `ρ_dict` is forced during the execution of the DICT-SCOPE `∀i` loop. `Thunk::new_unevaluated` creates thunks without forcing them, and `eval_key` evaluates in `ρ_parent` (not `ρ_dict`), so key evaluation cannot trigger forcing of sibling value thunks. Therefore, by the time any thunk is subsequently forced, `ρ_dict.B` contains all string-keyed bindings. This is the analogue of Launchbury's (1993) heap allocation step, which adds all letrec bindings before evaluating the body.

**Key isolation invariant:** Key expressions evaluate in `ρ_parent`, not `ρ_dict`. This prevents key computation from depending on sibling values that are unevaluated thunks, ensuring key evaluation is deterministic regardless of entry order. Without this invariant, `[x: 1  [call $x]: 2]` would cause the key expression `[call $x]` to reference `x` from `ρ_dict`, creating a dependency on the sibling entry `x: 1` (an unevaluated thunk), which breaks key evaluation determinism. Key evaluation itself requires materialization of the key expression's result (to obtain a concrete `String` or `Int` key) — this is inherent materialization in the sense of §Selective Materialization, since the key's identity must be known to populate `dict_map`.

**Computed keys cannot reference sibling entries.** Because keys evaluate in `ρ_parent`, a computed key like `$k` in `[k: hello  $k: 42]` resolves `k` via `ρ_parent`, not the dict's own letrec scope `ρ_dict`. If `k` is not bound in any enclosing scope, this is an unbound-variable error. This is intentional: allowing computed keys to see the dict's own bindings would create order-dependent key evaluation (the key at position 2 depends on the binding at position 1, which hasn't been evaluated yet during key computation). The key isolation invariant is strict — no exceptions for "earlier" entries.

**Letrec sharing invariant:** All value thunks `θᵢ` capture `ρ_dict` — the same mutable environment. When any `θᵢ` is forced, it evaluates in `ρ_dict`, which by then contains bindings for all string-keyed siblings (guaranteed by the construction-time non-forcing invariant). This is the mechanism behind mutual recursion: `even?` and `odd?` both capture the same `ρ_dict` and can reference each other through it.

**Referential integrity:** For any string-keyed entry `sᵢ ↦ θᵢ`, the thunk accessible via `lookup(sᵢ, ρ_dict)` (scope chain) and via `dict_map[String(sᵢ)]` (dict field access) is the same `Rc<Thunk>` identity (`eval.rs:348-353` uses `Rc::clone`). Forcing either access path memoizes the result for both — there is no divergence between `$x` within a dict and `.x` access on the dict from outside.

**[SEQ-SCOPE]** — Sequential expression scope chain within a document

```
Base case:
  exprs = []                                   (empty document)
  ────────────────────────────────────────────
  eval_document([], ρ_input, d) ⇒ Materialized(Dict([]))

Recursive case:
  exprs = [e₁, ..., eₙ]                       (document expressions, n ≥ 1)
  ρ₀ = ρ_input                                (initial scope — typically builtins + $$)

  ∀i ∈ 1..n-1:                                (intermediate expressions)
    θᵢ = eval(eᵢ, ρᵢ₋₁, d)
    vᵢ = force(θᵢ, d)                         (intermediate results are materialized)
    vᵢ = Dict(mapᵢ)                           (intermediate must be Dict — type error otherwise)
    ρᵢ = ({}, Some(ρᵢ₋₁))                    (fresh child env linked to prior scope)
    ∀(k, θ) ∈ mapᵢ:
      k = String(s) ⟹ ρᵢ.B[s] ← θ           (string keys become bindings)
      k = Int(_)    ⟹ no binding              (int keys are positional only)

  θₙ = eval(eₙ, ρₙ₋₁, d)                     (last expression: lazy, any type)
  ────────────────────────────────────────────
  eval_document(exprs, ρ_input, d) ⇒ θₙ
```

When `n = 1`, the `∀i ∈ 1..0` range is empty and the rule reduces to `eval_document([e₁], ρ_input, d) ⇒ eval(e₁, ρ_input, d)` — a single expression is evaluated lazily with no scope chain construction.

**Intermediate materialization:** Expressions `e₁..eₙ₋₁` are forced to extract their dict bindings into the scope chain. This is inherent materialization — the scope chain construction itself requires knowing the dict's keys to create named bindings. Note that the thunks `θ` extracted from `mapᵢ` are inserted into `ρᵢ` *without further materialization* — only the dict structure is forced, not the individual entry values. Those values remain lazy and are forced only when accessed via `$name` in subsequent expressions. The last expression `eₙ` is returned as a lazy thunk, preserving tinct's call-by-need semantics.

**Dict-type constraint:** Intermediate expressions must evaluate to `Dict`. This is not a type system constraint (the type checker does not enforce it) but a runtime invariant. If `vᵢ` is not a `Dict`, evaluation fails with a type mismatch error.

**[DOC-PIPELINE]** — Document isolation via `$$`

```
documents = [doc₁, ..., docₘ]               (file documents separated by ---)
ρ_base = ρ_builtins                          (shared root scope)
θ₀ = input_thunk                             (external input or empty dict)
d = depth                                    (evaluation depth; 0 at top-level)

∀j ∈ 1..m:
  ρ_docⱼ = ({$ ↦ θⱼ₋₁}, Some(ρ_base))     (fresh scope with only $$ bound)
  θⱼ = eval_document(docⱼ.exprs, ρ_docⱼ, d)

────────────────────────────────────────────
eval_file(documents, ρ_base, input_thunk, d) ⇒ θₘ
```

The binding name is `$`; the user-facing syntax `$$` desugars to `VarRef("$")` (see Part 1). At top-level invocation `d = 0`, but when called from `$include` (`builtins.rs:1126`), `d = depth + 1`, propagating the depth counter into nested file evaluation.

Documents are totally isolated — `ρ_docⱼ` inherits only from `ρ_base` (builtins), not from prior documents' scope chains. Data flows exclusively through `$$` (`θⱼ₋₁`). This is the `let*` analog at the document level, but with no shared scope between documents.

**Lazy pipeline boundary:** `θⱼ₋₁` is passed to the next document without materialization. The `---` boundary does not force evaluation — the pipeline is lazy end-to-end. This follows from the thunk lifecycle: no forcing rule is triggered at the document boundary, and `θⱼ₋₁` retains its current thunk state (which may be `Unevaluated`, `PendingBuiltin`, or `Materialized`). See Semantic Commitment 4 in §Thunk Lifecycle — Formal Specification.

##### Part 3: Variable Lookup

Variable lookup walks the parent chain from the current environment upward, returning the first match. This single mechanism implements both letrec-internal lookup and cross-expression resolution.

**[LOOKUP]**

```
lookup(x, ρ):
  (1) x ∈ dom(ρ)         ⟹ return ρ.B[x]              (found in current scope)
  (2) ρ.parent = Some(ρ') ⟹ return lookup(x, ρ')       (recurse to parent)
  (3) ρ.parent = None     ⟹ return None                 (unbound variable)
```

The implementation (`Environment::get`, `value.rs:445-460`) converts the recursion to iteration for stack efficiency. The two formulations are equivalent because the parent chain is finite and acyclic (Property 4 below).

**Shadowing semantics:** When the same name `x` is bound in both `ρ` and an ancestor `ρ'`, clause (1) returns `ρ.B[x]` — the nearest binding wins. This is standard lexical shadowing, formalized as Property 1 below.

##### Part 4: Scope Properties

Five properties that hold for all well-formed tinct programs. Each property follows from the construction rules (Part 2) and lookup rule (Part 3). The proofs use the Launchbury (1993) heap model extended with Nakata & Hasegawa's (2009) treatment of cyclic references.

**Property 1: Shadowing Correctness**

*Statement:* If name `x` is bound in environment `ρ` at depth `d₁` and also in ancestor `ρ'` at depth `d₂ > d₁` in the parent chain, then `lookup(x, ρ)` returns `ρ`'s binding at depth `d₁`.

*Proof sketch:* By structural induction on the parent chain length. LOOKUP clause (1) returns immediately when `x ∈ dom(ρ)`, without inspecting ancestors. Since the parent chain has finite length (Property 4), the nearest binding is always reached first. The inductive step: if `x ∉ dom(ρ)`, LOOKUP recurses to `ρ.parent`, reducing the chain length by one. By the inductive hypothesis, the nearest binding in the remaining chain is returned. ∎

**Property 2: Mutual Visibility (Letrec)**

*Statement:* For a dict constructed by DICT-SCOPE with entries `{s₁, ..., sₙ}` (string keys), forcing any thunk `θᵢ` can resolve `$sⱼ` for all `j ∈ 1..n`, including `j = i`.

*Proof sketch:* By DICT-SCOPE, all `θᵢ = Unevaluated(eᵢ, ρ_dict)`. By the construction-time non-forcing invariant, no thunk is forced during DICT-SCOPE construction, so by the time any `θᵢ` is subsequently forced, `ρ_dict.B` contains `{s₁ ↦ θ₁, ..., sₙ ↦ θₙ}` — all string-keyed bindings are present. When `θᵢ` is forced, `eval(eᵢ, ρ_dict, d)` has access to `ρ_dict`, and `lookup(sⱼ, ρ_dict)` succeeds via LOOKUP clause (1) for any `j`. Self-reference (`i = j`) is valid because forcing `θᵢ` transitions it to `InProgress` — a subsequent self-reference triggers FORCE-CYCLE (§Thunk Lifecycle), producing a cycle error rather than diverging. Mutual reference (`i ≠ j`) succeeds provided `θⱼ` is not already `InProgress` (no transitive cycle). This matches Nakata & Hasegawa's (2009) operational treatment of cyclic call-by-need: the `InProgress` state acts as a blackhole, ensuring termination for all reference patterns. ∎

**Property 3: Heap Monotonicity**

*Statement:* The set of bindings reachable from any environment `ρ` is monotonically non-decreasing over the course of evaluation. No binding is ever removed or reassigned to a different thunk.

*Proof sketch:* The binding map is monotonic because: (a) DICT-SCOPE rejects duplicate keys before insertion (`eval.rs:336-338`), so each binding is inserted exactly once into an initially empty map; (b) SEQ-SCOPE inserts into freshly created empty environments, so no overwrite is possible; (c) no code path calls `Environment::insert` on scope-chain environments after construction. The `insert` API itself (`IndexMap::insert`) permits overwriting, but these three invariants prevent it. The thunks themselves may transition states (Unevaluated → Materialized), but the binding `name ↦ θ` is stable — the `Rc<Thunk>` pointer does not change, only the thunk's internal state. By the thunk lifecycle monotonicity theorem (§Thunk Lifecycle Part 1), thunk state transitions are irreversible. Therefore both the binding map and the thunk contents are monotonic. ∎

**Property 4: Scope Chain Acyclicity**

*Statement:* The *parent chain* from any environment `ρ` to the root `ρ_builtins` is a finite, acyclic path.

*Proof sketch:* By induction on environment construction. Base case: `ρ_builtins` has `parent = None` — no cycle. Inductive step: both DICT-SCOPE and SEQ-SCOPE create fresh environments via `Environment::with_parent(ρ_existing)`. The new environment's parent is an already-constructed environment. Since environments are allocated with `Rc::new(RefCell::new(...))` and the parent pointer is set once at construction to an existing environment, no environment can have itself as an ancestor. Formally: define depth `d(ρ)` as the number of parent links from `ρ` to `ρ_builtins` (so `d(ρ_builtins) = 0`). DICT-SCOPE and SEQ-SCOPE both satisfy `d(ρ_new) = d(ρ_parent) + 1`, so depth strictly increases. A cycle would require `d(ρ) > d(ρ)`, a contradiction. ∎

**Parent chain vs capture graph:** This property concerns the *parent chain* (`env.parent` links), which is the graph walked by LOOKUP. The *capture graph* (`thunk.env` links) does contain cycles in letrec scopes: `ρ_dict` holds thunks that close over `ρ_dict` itself (via `Rc::clone(&dict_env)` at `eval.rs:342`). These capture cycles do not affect LOOKUP termination (LOOKUP walks only parent links) or semantic correctness. They do prevent `Rc` deallocation of letrec environments (since `Rc` cannot collect cycles), which is a known memory management limitation addressed by the planned arena migration (§Allocation Strategy — Phased Approach).

**Property 5: Determinism**

*Statement:* For the pure subset of tinct (no I/O builtins such as `$include`), `eval_document(exprs, ρ, d)` produces the same result thunk for the same input tuple `(exprs, ρ, d)`, and `lookup(x, ρ)` returns the same thunk for the same name and environment.

*Proof sketch:* LOOKUP is deterministic by construction — it is a linear scan of a fixed chain with a deterministic stopping condition (first match or `None`). DICT-SCOPE processes entries in source order; key evaluation in `ρ_parent` is deterministic by induction (keys are expressions evaluated in an already-determined environment); duplicate detection is deterministic (insertion-order `IndexMap`). SEQ-SCOPE processes expressions in source order, materializing each intermediate result deterministically. The only potential source of non-determinism — letrec evaluation order — is resolved by lazy evaluation: thunks are created in source order but forced on demand, and Ariola & Felleisen's (1997) confluence theorem (for the storeless calculus, transferred to tinct's heap model via Launchbury's (1993) adequacy result) guarantees that the order of forcing does not affect the final value in the pure call-by-need calculus. Non-determinism enters only through `$include` (file system I/O), which is outside the pure subset. ∎

**Depth and FORCE-DEPTH:** Determinism holds for the full input tuple `(exprs, ρ, d)` — depth `d` is part of the input, not ambient context. The same thunk may produce different results when forced at different depths (FORCE-DEPTH is the only forcing rule that does not transition thunk state — see Semantic Commitment 3 in §Thunk Lifecycle). This is not non-determinism but context-sensitivity: `eval_document` with a fixed `d` is a deterministic function. After the CEK migration removes MAX_EVAL_DEPTH, this caveat becomes moot.

##### Part 5: Implementation Correspondence

The formal rules map directly to the implementation:

| Formal rule | Implementation | Source |
|------------|----------------|--------|
| DICT-SCOPE | `eval_dict()` | `eval.rs:309-352` |
| SEQ-SCOPE | `eval_document()` | `eval.rs:199-249` |
| DOC-PIPELINE | `eval_file_with_input()` | `eval.rs:281-307` |
| LOOKUP | `Environment::get()` | `value.rs:445-460` |
| Key isolation | `eval_key(key_expr, parent_env, d)` | `eval.rs:327` |
| String-key filter | `if let Key::String(name) = key` | `eval.rs:234, 347` |
| Letrec sharing | `Thunk::new_unevaluated(expr, dict_env)` | `eval.rs:340-344` |
| Cycle detection | `InProgress` → FORCE-CYCLE | §Thunk Lifecycle — Formal Specification, Part 2: Forcing Rules |

**Deviations from Launchbury (1993):** Launchbury's original semantics threads an explicit heap `Γ` through all judgments: `Γ : e ⇓ Γ' : v`. tinct uses mutable `Rc<RefCell<Environment>>` instead of an explicit heap, which is operationally equivalent but obscures heap threading in the formal presentation. The correspondence is: Launchbury's `Γ[x ↦ e]` = tinct's `env.borrow_mut().insert(x, thunk)`, and Launchbury's `Γ(x)` = tinct's `env.borrow().bindings.get(x)`. The mutable cell model is standard in implementations (GHC uses a similar approach via `IORef` for thunk update).

**Deviations from Nakata & Hasegawa (2009):** Nakata & Hasegawa prove that cyclic call-by-need with blackholing (InProgress detection) terminates for all terms, producing either a value or a cycle error. tinct's `InProgress` state is exactly their blackhole. The deviation is that tinct additionally caches cycle errors via the `Failed` state (§Thunk Lifecycle), which Nakata & Hasegawa do not address — their semantics re-evaluates on each access. tinct's error memoization is a conservative extension: it preserves the value/error distinction while avoiding redundant cycle detection.

**Type system parallel:** The type checker builds a parallel scope chain (`TypeEnv`) mirroring the runtime scope chain. Within a dict (letrec), bindings are monomorphic during inference — type variables are not generalized until the entire dict is checked, matching the standard restriction on polymorphic recursion in HM (§Type Inference Algorithm). Between sequential expressions, dict boundaries increment `ℓ_current` for sound let-generalization (§Let-Generalization), and type schemes (not bare types) are threaded through the sequential scope chain to preserve polymorphism across expression boundaries. The full type-level formalization is in §Type Inference Algorithm and §Let-Generalization; this spec covers only runtime scope semantics.

##### Part 6: Worked Examples

**Example 1: Letrec mutual recursion**

```lisp
[
  even?: [fn [n] [call $if [call $= $n 0] true  [call $odd?  [call $- $n 1]]]]
  odd?:  [fn [n] [call $if [call $= $n 0] false [call $even? [call $- $n 1]]]]
  result: [call $even? 4]
]
```

DICT-SCOPE creates `ρ_dict` with parent `ρ_builtins`:
- `ρ_dict.B = {even? ↦ θ₁, odd? ↦ θ₂, result ↦ θ₃}` where all `θᵢ = Unevaluated(eᵢ, ρ_dict)`
- Forcing `θ₃` evaluates `[call $even? 4]` in `ρ_dict`
- `lookup(even?, ρ_dict)` → `θ₁` (clause 1) → forces `θ₁` → creates closure capturing `ρ_dict`
- The closure body references `$odd?` → `lookup(odd?, ρ_dict)` → `θ₂` (clause 1) ✓ mutual visibility
- Evaluation terminates: `even?(4) → odd?(3) → even?(2) → odd?(1) → even?(0) → true`

**Example 2: Sequential scope chain with shadowing**

```lisp
[x: 10  double: [fn [n] [call $* $n 2]]]

[x: 20  y: [call $double $x]]
```

SEQ-SCOPE with `ρ₀ = ρ_builtins`:
1. Evaluate `e₁` in `ρ₀` → DICT-SCOPE creates `ρ_dict₁` with `{x ↦ θ_10, double ↦ θ_fn}`
2. Materialize → `Dict({x: θ_10, double: θ_fn})`
3. Create `ρ₁ = ({x ↦ θ_10, double ↦ θ_fn}, Some(ρ₀))`
4. Evaluate `e₂` in `ρ₁` → DICT-SCOPE creates `ρ_dict₂` with parent `ρ₁`:
   - `ρ_dict₂.B = {x ↦ θ_20, y ↦ θ_call}`
   - `lookup(double, ρ_dict₂)`: not in `ρ_dict₂` → parent `ρ₁` → found ✓ (Property 1: `x` in `ρ_dict₂` shadows `x` in `ρ₁`)
   - `lookup(x, ρ_dict₂)` → `θ_20` (local, clause 1 — shadows `ρ₁`'s `x`)
5. Return `θ_last` (the thunk for `e₂`, lazy)

**Example 3: Document pipeline with `$$`**

```lisp
[base_port: 8080]

---

[port: [call $+ $$.base_port 1]]
```

DOC-PIPELINE (with `d = 0`):
1. `ρ_doc₁ = ({$ ↦ θ_empty}, Some(ρ_builtins))`. Evaluate doc₁ → `θ₁ = Dict({base_port: θ_8080})`
2. `ρ_doc₂ = ({$ ↦ θ₁}, Some(ρ_builtins))`. Note: `ρ_doc₂` has NO access to `ρ_doc₁`'s bindings — `$base_port` would fail. Data flows only through `$$`.
3. `$$.base_port` resolves: `lookup($, ρ_doc₂)` → `θ₁` (the variable name is `$`, spelled `$$` in source), then access chain `.base_port` on the dict.

#### Between Documents: Total Isolation via `$$`

`---` separates independent documents. Documents have no shared scope — as if they were in separate files.

Data flows between documents via `$$`, a variable injected into each document's root scope containing the previous document's output. For the first document in a file, `$$` is `[]` (empty dict). `$$` is VarRef("$") -- no grammar special case needed since `$` is a valid identifier character under the denylist rules.

**`$$` typing is context-dependent.** The static type of `$$` varies: it is an empty closed record `[]` when no stdin is provided (first document, no pipeline input), or `Any` when stdin JSON is parsed via `from-json` (since the JSON shape is unknown at compile time). The type checker assigns `$$` its type based on the evaluation context, but the static type system cannot capture the full range of runtime shapes `$$` may take. This is a known limitation — `[@Type $$]` type assertions are the escape hatch for narrowing `$$` to a specific record type.

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

LLT is a pure data transformation language with no in-language side effects, modulo `$include`, which performs filesystem I/O as a controlled side effect with sandboxing (similar to Nix's `import` and Dhall's `import`). The program evaluates to a value; the CLI serializes it:

```
tinct eval file.llt              # evaluate, output result as JSON
tinct eval -f yaml file.llt      # output as YAML (not yet implemented -- deferred)
tinct eval --eval file.llt       # deep-force all thunks before serializing (surfaces errors before partial output)
tinct eval -                     # read LLT source from stdin
cat data.json | tinct eval file.llt  # stdin JSON parsed and injected as $$ for first document
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

### Sandboxing & Security

LLT uses four unprivileged sandboxing layers to restrict what evaluation can access. All work without root privileges. Sandbox flags are global (before the subcommand), since a single `tinct` invocation runs exactly one subcommand.

#### Filesystem Sandbox (Application-Level + Landlock)

Two layers of defense for `$include` and any future file I/O:

**Application-level allowlist:** `$include` checks resolved paths against an allowlist before reading. All paths are canonicalized first (resolving symlinks and `../` traversal), then checked using path-ancestor matching: `canonical.ancestors().any(|a| allowed_paths.contains(a))`. This is path-element-based, not substring-based — `/tmp/allowed` does not match `/tmp/allowed2`. This is the primary control — works on all platforms, produces clear error messages.

**Landlock (Linux 5.13+):** Kernel-enforced filesystem ACLs as defense-in-depth. If a bug bypasses the application-level check, Landlock catches it at the kernel level. Detected at runtime; gracefully degrades on older kernels or non-Linux platforms (logs a warning, falls back to application-level check as the sole barrier).

**Allowlist model:**
- `--allow-path <dir>` adds a directory tree to the allowlist. Repeatable. Global flag.
- Default: `--allow-path .` (current working directory subtree). Project files accessible, nothing else.
- `--allow-path /` disables filesystem sandboxing entirely.
- Absolute paths in `$include` are allowed if they resolve within any allowed path.
- Symlinks: canonicalize to real path, then check. Symlinks pointing outside all allowed paths fail.
- `--allow-path` values are themselves canonicalized at CLI parse time (once), not on every include check.
- Stdlib is embedded via `include_str!` at compile time — no filesystem access, unaffected by sandboxing.
- REPL: default allow-path is cwd. LSP: workspace root (or document directory if no workspace).

**Check ordering in `$include`:** canonicalize path → allowlist check → cache check → cycle detection → read file. The allowlist check happens after canonicalization (to prevent symlink bypasses) but before the cache check (to prevent cached results from bypassing updated allowlists).

**Error message format:** `"include: path '/etc/passwd' is outside allowed paths (allowed: ['/home/user/project'])"` — shows resolved path and the allowlist so the user knows exactly what happened and how to fix it.

```bash
tinct --allow-path . eval main.llt                           # default (explicit)
tinct --allow-path ./lib --allow-path /shared eval main.llt  # explicit allowlist
tinct --allow-path / eval main.llt                           # unrestricted
```

#### Network Sandbox (seccomp-bpf)

No network features exist yet, but the sandbox is designed so future features (`$fetch`, remote includes) have a security model ready.

- Default: network blocked. seccomp-bpf blocks `socket`, `connect`, `bind`, `listen`, `accept` syscalls. Even if a future vulnerability allows code injection, the process cannot make network connections.
- `--allow-network` lifts the restriction (for future network features). Global flag.
- `--allow-host <host:port>` for fine-grained control (future — requires application-level checking since seccomp cannot filter by host).
- Seccomp filter installed in `main()` before evaluation starts (process-level, not per-eval).
- Linux-only; on other platforms, network features are controlled at the application level. Logs a warning on non-Linux.

#### Resource Sandbox (rlimit)

Prevents evaluation from consuming unbounded resources (DoS protection, runaway recursion). Uses POSIX `setrlimit` — works on Linux, macOS, and BSDs.

| Limit | Default | CLI Override | Applies to |
|-------|---------|-------------|------------|
| `RLIMIT_AS` | 512MB | `--max-memory 1G` | All subcommands |
| `RLIMIT_CPU` | 30s | `--max-cpu 60` | `eval` only |
| `RLIMIT_NOFILE` | 64 | `--max-fds 128` | All subcommands |
| `RLIMIT_FSIZE` | 10MB | — | All subcommands |

`RLIMIT_CPU` applies only to `eval`. The `lsp` and `repl` subcommands are long-lived processes where cumulative CPU time is expected — a 30-second CPU cap would kill them during normal use. Memory and file descriptor limits still apply to all subcommands as safety nets.

#### Process Sandbox (seccomp-bpf)

LLT is a pure configuration language — it should never spawn child processes.

- Always on. Blocks `fork`, `execve`, `execveat` via seccomp. `clone` is allowed because LLT uses worker threads (64MB stack for pest deep nesting workaround).
- No CLI flag to disable — there is no legitimate reason for a config evaluator to fork or exec.
- Linux-only; on other platforms, LLT simply never calls process-creation APIs. Logs a warning on non-Linux.

#### Initialization Order

Sandbox setup in `main()` follows this sequence:

1. Parse CLI (clap) — get `--allow-path`, `--max-memory`, etc.
2. Set up rlimit (resource caps)
3. Set up seccomp-bpf (network block, process block)
4. Set up Landlock (filesystem ACLs from `--allow-path`)
5. Load stdlib (`create_stdlib_env()` — uses `include_str!`, no filesystem access)
6. Dispatch subcommand (eval/repl/lsp)

Seccomp and Landlock are applied before any evaluation. `prctl(PR_SET_NO_NEW_PRIVS)` is called before seccomp installation.

#### Platform Support

| Sandbox | Linux | macOS | Windows |
|---------|-------|-------|---------|
| Filesystem (application) | Yes | Yes | Yes |
| Filesystem (Landlock) | 5.13+ | No | No |
| Network (seccomp) | 3.5+ | No | No |
| Resources (rlimit) | Yes | Yes | No |
| Process (seccomp) | 3.5+ | No | No |

On non-Linux platforms, the application-level filesystem check and rlimit (where available) provide the core security guarantees. seccomp and Landlock are defense-in-depth layers specific to Linux. When unavailable, a warning is logged and the application-level checks remain the sole barrier.

#### EvalConfig Integration

The filesystem allowlist lives in `EvalConfig` (immutable per evaluation session):

```rust
struct EvalConfig {
    base_dir: PathBuf,
    stdlib_env: Rc<RefCell<Environment>>,
    allowed_paths: Vec<PathBuf>,    // canonicalized at CLI parse time
    // future: allowed_hosts: Vec<String>,
}
```

`$include` checks `config.allowed_paths` before reading. Landlock, seccomp, and rlimit are set up in `main()` before evaluation starts — they are process-level restrictions, not per-eval.

#### Rust Crates

- `landlock` — official Landlock LSM wrapper
- `seccompiler` — seccomp-bpf filter builder (from rust-vmm/Firecracker)
- `rlimit` — setrlimit wrapper

### Formatter (`llt fmt`)

**Zero-configuration** code formatter for LLT files. Operates on the hand-written lexer's token stream (not the AST), so comments and whitespace are preserved and reformatted.

**Architecture:** The formatter lexes source into a token stream (including comment tokens), groups tokens into bracket-delimited blocks, applies formatting rules, and emits reformatted source. It does not parse to AST — this avoids losing comments (pest silently drops them) and avoids a dependency on the iterative parser.

#### Line-Breaking: Width + Element Count

A bracket expression `[...]` is rendered on a single line if both conditions are met:
1. The fully-expanded single-line form fits within **80 characters** (including indentation)
2. The expression contains **≤ 4 entries** (key-value pairs or positional values)

If either condition fails, the expression is expanded to one entry per line, indented 2 spaces deeper than the opening bracket. There is no middle ground — expressions are either fully collapsed or fully expanded.

**Exception:** Function parameter lists (`[fn [params...] body]`) and function type parameter lists (`Fn@Return [Params]`) are always rendered on a single line regardless of width, since splitting params across lines hurts readability.

**Entry counting:** The element count applies to the immediate bracket level, not recursively. A nested bracket like `[@[type: Number default: 0] $expr]` counts as 2 entries at the outer level (the annotation dict and `$expr`), regardless of how many entries the inner `[type: Number default: 0]` contains. Each `...` or `...name` rest entry counts as one entry.

**Rationale:** Width-only (gofmt-style) produces unreadable dense lines for dicts with many short entries. Optimal-layout algorithms (Wadler-Lindig) are overkill for LLT's relatively flat structure. The element count cap of 4 matches the existing stdlib conventions.

#### Comment Attachment: Line-Affinity

Comments are attached to code based on their line position:

- **Trailing comment:** A `#` comment on the same line as code stays attached to that code. `x: 5  # the x value` → the comment is part of the `x: 5` entry.
- **Leading comment:** A `#` comment on its own line is attached to the next code line. It is indented to match the code it precedes.
- **Section comment:** A blank line before a leading comment breaks the attachment — the comment becomes a standalone section separator. The blank line is preserved.

#### Semicolons: Always Removed

Semicolons are normalized away. They are syntactic sugar for newlines, and the formatter emits the canonical whitespace-separated form. `[x: 1; y: 2]` becomes `[x: 1 y: 2]` (single-line) or two separate lines (multi-line). The stdlib uses zero semicolons — this is the canonical style.

#### Configurability: Zero-Config

No formatting options. The formatter defines the canonical LLT style. The only CLI flags control I/O behavior:
- `--check` — exit 1 if any file is not formatted (CI mode)
- `--in-place` — overwrite files in place
- `--stdin` — read from stdin, write to stdout

**Rationale:** gofmt's zero-config philosophy. One canonical style eliminates bikeshedding. Pre-1.0, if a genuine need for configurability emerges, knobs can be added later. Starting opinionated is easier than tightening.

#### Additional Rules

| Rule | Behavior |
|------|----------|
| Indentation | 2 spaces per bracket depth, fixed |
| Key-value spacing | One space after `:` — `key: value` |
| Access chains | Never broken across lines — `$a.b[0].c` stays intact |
| `---` separators | One blank line above and below (no blank before first document) |
| Blank lines | Collapse runs of 2+ to 1. Preserve single blank lines (intentional grouping) |
| Trailing whitespace | Stripped on every line |
| Trailing newline | Single newline at end of file |
| `@` annotations | No spaces around `@` — `x@Number`, `Fn@Return`, never `x @ Number` |
| Quoted strings | Preserved exactly (escapes not normalized; idempotency) |
| Comments in access chains | Cannot occur (compound-atomic grammar); formatter does not handle |

### Dual-Dispatch Builtins Typed as `Any`

**Dual-dispatch operations** (`$map`, `$filter`, `$take`, `$drop`, `$reduce`, `$join`) accept both Dict and Seq inputs and produce different output types depending on the input. The type checker assigns these builtins type `Any` because:

1. LLT has no union types — the precise input type `Dict | Seq` cannot be expressed
2. Separate functions (`$map-dict`, `$map-seq`) would be verbose and break the polymorphic API
3. Overloaded function types would require type system extensions (type classes or similar)
4. `Any` is already used for other inherently dynamic operations (e.g., `$from-json`)

Type assertions (`[@Type $expr]`) provide a runtime narrowing mechanism when concrete types are needed. This decision will be revisited if the type system gains union types or type classes in future phases.

### Type System Extension Roadmap

The type system evolves in two scheduled phases and one gated phase. Each phase is independently useful and produces a complete type system.

**Phase 1 — Precision.** Register builtin type signatures, add Seq type inference, add error recovery for LSP.

- `TypeEnv::with_builtins()` constructor pre-registering type signatures for all 44 Rust-native builtins. Dual-dispatch builtins (`$map`, `$filter`, etc.) are typed as `Any` (matching §Dual-Dispatch Builtins above). Non-overloaded builtins get precise types (e.g., `$+ : Fn(Number, Number → Number)`, `$length : Fn(Any → Int)`).
- Seq type inference for sequence-only builtins (`$seq`, `$range`, `$repeat`, `$cycle`, `$iterate`, `$unfold`, `$take`). Annotate return types in `check_call` so LSP hover shows `Seq(Int)` instead of `Any`. Dual-dispatch builtins (`$map`, `$filter` on Dict|Seq) remain typed as `Any` — precise typing requires type classes or union types (Phase 3).
- `Type::Error` sentinel — a type that propagates silently through inference without generating additional errors. When a subexpression fails type checking, `Type::Error` prevents cascading errors (currently, a single type error can produce 5–10 follow-on errors from dependent expressions). Semantics: `unify(Error, τ) → S` unchanged (no binding, no error), `is_subtype(Error, _) = false`. `Type::Error` is recorded in the type map so LSP hover can show "error" rather than nothing. This is the standard approach used by GHC, Elm, and Rust.

Phase 1 does not change any inference rules or subtyping relationships. It extends the type environment and improves error reporting.

**Phase 2 — Completeness.** Extend type inference to cover named arguments, detect polymorphic recursion, and fix the function variance inconsistency.

- Named arg unification — extend `Type::Function` to carry param names: `Function { params: Vec<(Option<String>, Type)>, ret: Box<Type> }`. Named args are matched **positionally** (by index, matching current evaluation semantics where named args fill positional parameter slots). After positional unification, named args are unified against their corresponding param types. This resolves the "named args not type-checked" limitation.
- Polymorphic recursion detection — forbid with a clear error message ("polymorphic recursion requires explicit type annotation"), rather than silently diverging during inference. Detection is immediate (depth 1): if a recursive call site instantiates a type variable that was bound by an outer call to the same function, report the error. No partial polymorphic recursion is allowed. This item assumes let-generalization (§Let-Generalization) is implemented — without let-polymorphism, every recursive call is monomorphic by definition and the detection is vacuous.
- Function variance fix — the current dual-path design (unify for CALL-POLY, is_subtype for CALL-MONO) gives different verdicts for the same type relationship depending on whether type variables are present. The structural recursive `check_expr` from the bidirectional typing design (§Type Inference Algorithm, bidirectional checking rules) resolves this by applying [SUB] at leaves and unification only at actual type variable positions.
- Formalize `Any` semantics (documentation only) — document the consistency relation that `Any` actually implements, distinguishing it from true subtyping. Define what the Gradual Guarantee means for tinct. Identify blame boundaries (TypeAssert, builtin return types, function annotations). This is preparatory work for Phase 3 gradual typing, done as documentation in DESIGN.md, not code. Scope is limited to documenting current behavior and identifying where it diverges from formal gradual typing — it does not specify target semantics (that is Phase 3). See `doc/whatif/gradual-typing.md` for the full analysis.

Phase 2's named arg unification depends on Phase 1 (builtin type signatures must exist before named args can be checked against them). Other Phase 2 items (polymorphic recursion detection, function variance fix, `Any` formalization) may proceed in parallel with Phase 1.

**Relationship to other sprints.** The row-unification sprint (§Row-Variable Unification) and let-generalization (§Let-Generalization) are separate infrastructure sprints, not part of this roadmap. Phase 2's polymorphic recursion detection assumes let-generalization is implemented. Row variable binding is arguably more impactful than any single roadmap item — without it, row polymorphism annotations exist syntactically but row variables are never bound during inference.

**Phase 3 — Expressiveness (gated, not scheduled).** Three independent features, each triggered by a specific condition. These are research-level extensions analyzed in `doc/whatif/` files.

| Feature | Gate | Analysis |
|---------|------|----------|
| Gradual typing formalization | `Any`-as-top-and-bottom causes a soundness bug that affects users | `doc/whatif/gradual-typing.md` |
| Type classes | User-defined types need to participate in builtin protocols (Eq, Ord, Num). Presupposes a user-defined type mechanism, which is not currently planned. | `doc/whatif/typeclasses.md` |
| Union types | `Any` typing for dual-dispatch builtins causes false positives in practice | `doc/whatif/union-types.md` |

Phase 3 features are independent of each other — any can be adopted without the others. The `doc/whatif/` files analyze what each adoption would require, what it would gain and lose, and recommend an implementation approach if the gate is triggered.

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
| `if` | **Current:** Materializes condition and the chosen branch. **Phase 5b** (see §Current vs Planned Laziness Analysis)**:** Will return the chosen branch as a thunk (other never materialized). |
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
| `get-in`, `get-in-or` | **Materializing** — deep path access. Takes a dict and a list of keys, traverses nested dicts. Must evaluate each key lookup. `get-in-or` returns a default on missing keys instead of erroring. |
| `set`, `remove` | Structural — add/remove entries |
| `merge` | **Current:** Eagerly clones both input dicts. **Phase 5b** (see §Current vs Planned Laziness Analysis)**:** Will use lazy overlay (right dict's keys shadow left dict's keys, no deep copy until access). |
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

**Rust-native builtins (44 total):**

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
| Sequences | `seq`, `head`, `tail`, `collect`, `seq?`, `range`, `repeat`, `cycle`, `iterate`, `unfold`, `take`, `map`, `filter`, `drop`, `reduce`, `join` | `seq` constructs lazy cons cells; `head`/`tail` extract without materializing tail; `collect` converts Seq to dict with integer keys; `seq?` type predicate. Sequence constructors (`range`, `repeat`, `cycle`, `iterate`, `unfold`) return infinite or finite Seq (O(1) construction). `take`, `map`, `filter`, `drop` are dual-dispatch: on Dict preserve keys, on Seq return Seq. `reduce` accumulates with early termination on empty Seq. `join` converts Dict/Seq to string with separator (O(n²) concatenation). All require `Rc<Thunk>` manipulation unavailable in LLT. |
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
| `words` | `[call $filter [fn [w] [call $not [call $= $w ""]]] [call $split " " $s]]` | `split` + `filter` |

Note: `and` and `or` are also LLT-derived (`[fn [a b] [call $if $a $b false]]` works via lazy args, giving short-circuit semantics for free). Similarly, `get` is just `[fn [xs k] $xs[$k]]` using bracket access.

**LLT-implemented stdlib:**

Everything else is implemented in LLT using the Rust builtins above plus language features (bracket access, dict literals, `fn`, `call`, recursion via letrec). Key implementation patterns:

- **Derived primitives**: `not` from `if`, comparison operators from `<`, `mod` from arithmetic, `ceil`/`trunc` from `floor`, `words` from `split`.
- **Short-circuit logic** via lazy args: `and` = `[fn [a b] [call $if $a $b false]]`.
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
Rust builtins ($+, $-, $<, $=, $if, $keys, $merge, $str, $floor, $map, $filter, $reduce, $drop, $join, ...)
  └── LLT stdlib ($not, $>, $mod, $ceil, $and, $fold, $compose, ...)
        └── User code
```

### Stdlib Function Reference (62 functions)

Functions available to all user code. Most are implemented in LLT in `stdlib/prelude.llt`; some performance-critical operations (`map`, `filter`, `reduce`, `drop`, `join`, `take`, and all sequence constructors) are Rust-native builtins with dual-dispatch on Dict vs Seq. Private implementation details (functions suffixed with `-impl`) are omitted.

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
| `join` | `[fn [sep xs] ...]` | Join values as strings with separator (Rust builtin, O(n); dual-dispatch Dict/Seq) |
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
| `get-in` | `[fn [xs path] ...]` | Traverse nested dicts by a list of keys; errors on missing key |
| `get-in-or` | `[fn [xs path default] ...]` | Traverse nested dicts with fallback default |
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
| `filter` | `[fn [pred xs] ...]` | Keep values where predicate returns true (returns Seq) |
| `reduce` | `[fn [f init xs] ...]` | Left fold (Rust builtin; dual-dispatch Dict/Seq) |
| `fold` | `[fn [f init xs] ...]` | Alias for `reduce` |
| `slice` | `[fn [xs start end] ...]` | Positional slice (start inclusive, end exclusive) |
| `take` | `[fn [n xs] ...]` | Take the first n entries, preserving keys |
| `drop` | `[fn [n xs] ...]` | Skip first n entries (Rust builtin; dual-dispatch Dict/Seq) |
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
| `range` | `[fn [start] ...]` or `[fn [start end] ...]` | Seq of integers from start (inclusive); infinite if 1-arg, finite (end exclusive) if 2-arg |
| `repeat` | `[fn [val] ...]` | Infinite Seq of copies of val; for finite, use `[call $take n [call $repeat val]]` |
| `cycle` | `[fn [xs] ...]` | Infinite Seq cycling through dict entries; for finite, use `[call $take n [call $cycle xs]]` |
| `iterate` | `[fn [f x] ...]` | Infinite seq: x, f(x), f(f(x)), ... |
| `unfold` | `[fn [step seed] ...]` | Seq from step function; step returns `[value state]` or `[]` to stop |
| `take` | `[fn [n xs] ...]` | Dual-dispatch: on Dict, take first n entries preserving keys; on Seq, return finite Seq of first n elements |
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
| `range` | `[call $range 0 10]` | `[call $range 0]` | Integers from start (inclusive); 2-arg has end (exclusive), 1-arg is infinite |
| `repeat` | `[call $take 5 [call $repeat x]]` | `[call $repeat x]` | Infinite Seq of val; use `take` for finite |
| `cycle` | `[call $take 3 [call $cycle xs]]` | `[call $cycle xs]` | Infinite Seq cycling through dict entries; use `take` for finite |
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

#### Productivity Obligations

**Sequences are coinductive** — they are defined by observations (head/tail), not by construction (Coquand 1994). A sequence is **productive** if every observation step terminates: taking the head yields a value, and forcing the tail yields another sequence (or `[]`).

**tinct makes no static productivity guarantee.** This is a deliberate choice, shared by every practical lazy language with general recursion (Haskell, Nix, Nickel, Jsonnet). Static productivity checking requires either totality (Turner 2004, Dhall's approach — Turing-incomplete) or sized types (Abel & Pientka 2013, Abel 2012 — require constraint solving beyond HM unification, incompatible with tinct's type inference). Guardedness alone is insufficient: Coquand's proof that guardedness implies productivity assumes all sub-computations terminate, which general recursion does not guarantee. Sequence constructors (`$seq`, `$range`, `$repeat`, etc.) currently infer as `Type::Any` — `Type::Seq` inference in `typecheck.rs` is tracked as future work in the type-extensions sprint.

**Three layers of runtime protection:**

| Layer | Mechanism | What it catches |
|-------|-----------|----------------|
| Blackholing | `InProgress` thunk state sentinel | Direct cycles: a thunk that references itself during evaluation |
| Depth limit | `MAX_EVAL_DEPTH=256` | Runaway recursion: deeply nested or diverging evaluation chains |
| Tail discipline | `$collect`/`$head`/`$tail` type checks | Malformed tails: sequence tail that evaluates to a non-Seq, non-`[]` value |

**Built-in constructors are productive by construction.** The standard sequence API guarantees productivity for well-behaved arguments:

| Constructor | Productivity guarantee |
|-------------|----------------------|
| `$range` | Always productive (generates integers) |
| `$repeat` | Always productive (repeats a constant) |
| `$cycle` | Productive if input dict is non-empty |
| `$iterate` | Productive if `f` terminates on every input |
| `$unfold` | Productive if step function terminates on every state |
| `$map` on Seq | Productive if source is productive and `f` terminates |
| `$filter` on Seq | Productive if source is productive, predicate terminates, **and infinitely many elements pass** (or source is finite) |

**`$seq` is the raw constructor with user-managed obligations.** `[call $seq $head $tail]` wraps two thunks into a Seq without forcing either. This enables guarded corecursion:

```lisp
ones: [call $seq 1 $ones]
# Works: $seq does NOT force $ones. The tail thunk captures $ones
# as an unevaluated reference. Each $tail observation produces a
# new Seq(1, <thunk>) without diverging.
```

`$seq` is lazy — it does not materialize its arguments (`builtins.rs:builtin_seq` wraps `Rc::clone(&args[0])` and `Rc::clone(&args[1])` directly). This is critical: it means `$seq` acts as a guard in the coinductive sense, allowing corecursive definitions that would cycle under eager evaluation.

**User obligations for `$seq`:**

1. The head thunk must terminate when observed.
2. The tail thunk must evaluate to either a `Seq` or `[]`.
3. Corecursive definitions must have at least one `$seq` constructor between the binding and the recursive reference (guardedness).

Violating these produces a runtime error (cycle detection or depth limit) for the failure modes tinct can detect. Slowly diverging computations (e.g., superpolynomial head evaluation) will appear to hang — this is inherent to any Turing-complete language without static totality.

**Why not static productivity checking:** Idris makes totality/productivity checking opt-in because mandatory checking rejects valid programs (Brady's rationale). Agda/Coq's mandatory guardedness is known to be fragile — it rejects intuitively productive programs, especially those using higher-order functions. Abel & Pientka's (2013) copatterns with sized types provide automatic productivity checking, but require size annotations threaded through the entire type system — constraint solving beyond HM unification. For a data transformation language, the pragmatic approach (productive-by-construction combinators + runtime backstop) provides the right tradeoff between safety and expressiveness.

**Error quality matters more than static checking.** Nix's biggest user-facing pain point with non-productive definitions is not the lack of static checking but the poor diagnostics ("infinite recursion encountered" with no useful context). tinct's error reporting should include: the thunk origin (which binding diverged), the materialization chain (who forced it), and the depth at which the limit was hit.

#### Thunk Lifecycle — Formal Specification

Extends Launchbury (1993) natural semantics for call-by-need with three additional thunk states (PendingBuiltin, PendingCall, Failed) for deferred computation and error memoization. PendingBuiltin and PendingCall are defunctionalized continuations (Reynolds 1972; Danvy & Nielsen 2003) — they represent deferred computation as data rather than closures.

**State set:** `S = { Unevaluated, PendingBuiltin, PendingCall, InProgress, Materialized, Failed }`

##### Part 1: State Transition DAG

The valid state transitions form a directed acyclic graph. Monotonicity theorem: all transitions move forward; no state ever reverts to a prior state.

```
Unevaluated ──────────┐
PendingBuiltin ────────┼──→ InProgress ──┬──→ Materialized
PendingCall ───────────┘                 └──→ Failed ⟲
```

The DAG governs state *transitions*, not construction. Thunks may be constructed directly in Unevaluated, PendingBuiltin, PendingCall, or Materialized state (via `Thunk::new_materialized`). The DAG applies only to subsequent state changes.

Transition rules (each maps to one `take_*` or `set_state` call in `src/value.rs`):

| Transition | Trigger | Atomicity |
|-----------|---------|-----------|
| Unevaluated → InProgress | `take_unevaluated()` | Atomic (`mem::replace`) |
| PendingBuiltin → InProgress | `take_pending_builtin()` | Atomic (`mem::replace`) |
| PendingCall → InProgress | `take_pending_call()` | Atomic (`mem::replace`) |
| InProgress → Materialized | `set_state(Materialized(v))` | Direct write |
| InProgress → Failed | `cache_failure(err)` | Via `transition()` |
| Failed → Failed | `set_state(Failed(e'))` | Direct write (diagnostic refinement only) |

**Monotonicity proof sketch:** The DAG has no cycles. Each source state (Unevaluated, PendingBuiltin, PendingCall) transitions only to InProgress. InProgress transitions only to Materialized or Failed. Materialized is terminal — no transitions out. Failed has a self-edge for diagnostic refinement (enriching materialization spans and stack frames), but the error's semantic identity is fixed — only diagnostic metadata may be updated. Therefore all transition sequences are finite, and the semantic content of a thunk is monotonically determined. ∎

**Atomicity invariant:** Each `take_*` method atomically swaps the thunk state to InProgress before returning the captured data. This ensures no observer can see the old state after the transition begins. The atomicity is provided by `std::mem::replace` under an exclusive `borrow_mut()` — Rust's borrow checker prevents double borrows within a single thread.

##### Part 2: Forcing Rules

Forcing (materialization) dispatches on the current state to produce a value or error. Rules use the judgment form `force(θ, d) ⇒ v` where θ is a thunk, d is the current depth, and v is the resulting value.

**Notation:** The rules use an implementation-oriented notation mixing imperative state updates (`θ.state ← InProgress`) with declarative judgments (`eval(expr, env, d+1) ⇒ θ'`). A standard operational semantics would thread an explicit store σ mapping thunk IDs to states: `force(θ, d, σ) ⇒ (v, σ')`. The notation here maps directly to the `materialize()` implementation for ease of cross-checking.

**Precondition:** FORCE-DEPTH is checked before state dispatch. All other rules implicitly have `d ≤ MAX_EVAL_DEPTH` as a precondition.

**[FORCE-DEPTH]**
```
d > MAX_EVAL_DEPTH
───────────────────────────
force(θ, d) ⇒ error("maximum evaluation depth exceeded")
θ.state unchanged             (depth is a stack property, not a thunk property; see Commitment 3)
```

FORCE-DEPTH does not update θ.state because the depth limit is context-dependent. The same thunk may succeed when forced at a lower depth. This is the only forcing rule that does not transition the thunk state — it is also the only rule that breaks determinism in the pure subset (the same thunk can produce different results depending on the call-site depth). After the CEK migration removes MAX_EVAL_DEPTH, this rule becomes moot.

**[FORCE-CACHED]**
```
θ.state = Materialized(v)
───────────────────────────
force(θ, d) ⇒ v
```

**[FORCE-FAILED]**
```
θ.state = Failed(e)
───────────────────────────
force(θ, d) ⇒ error(e')
```

The materialization span update has three cases (`eval.rs:876-896`): (1) if e has no materialization span and one is available, set it; (2) if the access span matches the existing materialization span, no-op; (3) if the access span differs and is not already in the stack, add it as a stack frame (preserving the original materialization span). This Failed → Failed diagnostic refinement is an intentional relaxation of strict idempotence at the error-representation level — the error's identity and root cause are fixed, but diagnostic annotations accumulate across access paths.

**[FORCE-CYCLE]**
```
θ.state = InProgress
───────────────────────────
force(θ, d) ⇒ error("circular dependency")
θ.state ← Failed(err)         (memoize the cycle error)
```

**[FORCE-EVAL]**
```
θ.state = Unevaluated(expr, env)
θ.state ← InProgress                          (blackhole)
eval(expr, env, d+1) ⇒ θ'
force(θ', d+1) ⇒ v
θ.state ← Materialized(v)                     (memoize)
───────────────────────────
force(θ, d) ⇒ v
```

**[FORCE-EVAL-ERR]**
```
θ.state = Unevaluated(expr, env)
θ.state ← InProgress
eval(expr, env, d+1) ⇒ θ'
force(θ', d+1) ⇒ error(e)
θ.state ← Failed(e)                           (memoize error)
───────────────────────────
force(θ, d) ⇒ error(e)
```

**[FORCE-BUILTIN]**
```
θ.state = PendingBuiltin(f, args, named, pd, cs)
θ.state ← InProgress
f(args, named, pd, cs) ⇒ θ'
force(θ', d+1) ⇒ v
θ.state ← Materialized(v)
───────────────────────────
force(θ, d) ⇒ v
```

The builtin receives `pd` (the pending depth captured at PendingBuiltin construction time) for its own recursion budget, but the subsequent `force(θ', d+1)` uses the current depth `d`. A PendingBuiltin created at depth 10 but forced at depth 200 runs the builtin with depth-context 10 but recurses at depth 201.

**Depth semantics rationale.** The two-depth design is intentional. `pd` (pending depth) governs the builtin's *internal* materialization budget — how deep the builtin itself may recurse when examining its arguments (e.g., `$merge` materializing both operands). The current depth `d` governs the *continuation* — how deep the result may be forced after the builtin returns. Using `pd` for the builtin preserves the depth budget the caller intended when constructing the PendingBuiltin; using `d` for the continuation reflects the actual call-stack depth at forcing time. This prevents a deeply-deferred PendingBuiltin from circumventing depth limits: the builtin runs with its original budget, but the result is forced at the current (possibly deeper) stack position. After CEK migration, depth tracking is replaced by explicit fuel/stack-size limits, and this two-depth distinction is eliminated.

**[FORCE-CALL]**
```
θ.state = PendingCall(f_θ, args, named, cs)
θ.state ← InProgress
force(f_θ, d+1) ⇒ Function(params, body, env)
invoke(params, body, env, args, named) ⇒ θ'
force(θ', d+1) ⇒ v
θ.state ← Materialized(v)
───────────────────────────
force(θ, d) ⇒ v
```

**[FORCE-CALL-BUILTIN]**
```
θ.state = PendingCall(f_θ, args, named, cs)
θ.state ← InProgress
force(f_θ, d+1) ⇒ Builtin(func)
func(args, named, d, cs) ⇒ θ'
force(θ', d+1) ⇒ v
θ.state ← Materialized(v)
───────────────────────────
force(θ, d) ⇒ v
```

If `force(f_θ)` produces a value that is neither Function nor Builtin, the forcing fails with a type mismatch error (`eval.rs:1043-1049`), which is cached in Failed state.

Error variants for FORCE-BUILTIN, FORCE-CALL, and FORCE-CALL-BUILTIN follow FORCE-EVAL-ERR: on any error, `θ.state ← Failed(e)` before propagation.

**Error decoration:** All errors are decorated via `attach_materialization_context` (`eval.rs:803-831`) before caching, adding the materialization span (if not already set) and origin stack frames. The decoration happens in the `map_err(&decorate)` chain before `cache_failure` is called.

**Fast path:** In FORCE-BUILTIN, FORCE-CALL, and FORCE-CALL-BUILTIN, if θ' is already Materialized, skip the recursive `force` and extract the value directly. This is observationally equivalent to the general rule — FORCE-CACHED fires immediately on the recursive `force(θ', d+1)` — but avoids the function call overhead (`eval.rs:944-945` for PendingBuiltin, `eval.rs:1020-1022` for PendingCall).

##### Part 3: Semantic Properties

Six properties essential for call-by-need soundness (Launchbury 1993, Ariola & Felleisen 1997):

| Property | Status | Qualification |
|----------|--------|---------------|
| **Determinism** | Satisfied | Pure subset only; `$include` introduces external state dependence. FORCE-DEPTH is also context-dependent (same thunk may succeed at different depths) |
| **Sharing (evaluate-at-most-once)** | Satisfied | Materialized and Failed are semantically terminal — subsequent forces return cached result (Failed may refine diagnostic metadata) |
| **Monotonicity** | Satisfied | DAG has no backward edges; Failed self-edge refines diagnostics only (proven above) |
| **Adequacy** | Holds for extensions | PendingBuiltin/PendingCall are observationally equivalent to Unevaluated (defunctionalization preserves semantics). Failed extends the codomain from Value⊥ to Value + Error⊥ (absorbing, deterministic) |
| **Confluence** | Pure subset only | `$include` makes evaluation order observable; in the pure subset, forcing order does not affect final values |
| **Sharing preservation** | Satisfied | `Rc<Thunk>` ensures identity-based sharing; CEK migration must preserve thunk identity through continuation dispatch |

##### Semantic Commitments

Implicit decisions in the current implementation, made explicit:

**1. Error memoization is permanent.** Once a thunk reaches Failed, it never retries. This includes I/O failures from `$include` — a file-not-found error is cached forever, even if the file appears later. This is correct for a build-time evaluator (deterministic builds) and matches Nix's `nFailed` semantics (Peyton Jones et al. 1999 "imprecise exceptions"). Retryable failures would require a new `Retryable` state or external retry logic — not planned.

**2. Confluence holds only in the pure subset.** `$include` introduces evaluation-order dependence: if file A includes file B and file B includes file A, the result depends on which is evaluated first (cycle detection fires on the second). All other tinct operations are confluent — forcing order does not affect the result. The pure subset of tinct (no `$include`) satisfies the diamond property of Ariola & Felleisen's (1997) call-by-need calculus.

**3. MAX_EVAL_DEPTH is practical, not semantic.** The depth bound (256) is an implementation artifact to prevent stack overflow in the recursive evaluator. It is not part of the formal semantics — a correct implementation with sufficient stack space should produce the same values without the bound. The CEK machine migration (with heap-allocated continuations) should remove this bound, replacing it with configurable resource limits (`--max-depth`) if needed. Consequently, FORCE-DEPTH errors are non-destructive: the thunk state is unchanged, and the same thunk may succeed at a lower depth.

**4. Finite vs productive thunk lifecycles.** Dict-entry thunks have a **finite lifecycle**: they must eventually reach Materialized or Failed. Seq tail thunks have a **productive lifecycle**: materializing a tail yields a Seq value (containing a new tail thunk) or the terminal `[]`. The state machine is identical; the liveness obligation differs. This distinction is not enforced by the type system — it is a semantic contract between the sequence constructors and the programmer (see §Productivity Obligations).

##### Adequacy of PendingBuiltin and PendingCall

These states are defunctionalized continuations (Reynolds 1972). Each is observationally equivalent to an Unevaluated thunk holding an expression that would perform the same computation:

- `PendingBuiltin(f, args, named, d, cs)` ≡ `Unevaluated([call $f ...args ...named], env)` where env binds the arg thunks
- `PendingCall(f_θ, args, named, cs)` ≡ `Unevaluated([call <force f_θ> ...args ...named], env)`

The equivalence for PendingCall holds because `eval` of `[call ...]` already performs dynamic dispatch on the callee — if `f_θ` materializes to a Builtin rather than a Function, both the PendingCall path (FORCE-CALL-BUILTIN) and the hypothetical Unevaluated path would dispatch to the same builtin.

The difference is operational: PendingBuiltin/PendingCall avoid constructing AST nodes for deferred computations. A formal adequacy proof would show bisimulation: every forcing sequence starting with `PendingBuiltin(f, args, ...)` produces the same value as forcing `Unevaluated([call $f ...args], env)`. This is conjectured based on the defunctionalization correspondence (Reynolds 1972; Danvy & Nielsen 2003) but not mechanically verified — see TODO.md for mechanized proof obligations.

##### Relationship to CEK Machine Migration

The planned iterative evaluator (§Iterative Evaluator) subsumes PendingBuiltin and PendingCall into explicit `Cont` variants on the continuation stack. After migration:

- The ThunkState enum simplifies to `{Unevaluated, InProgress, Materialized, Failed}`
- PendingBuiltin and PendingCall become `Cont::BuiltinDispatch` and `Cont::CallForceFunc` on the explicit stack; both must handle Function and Builtin dispatch after forcing
- The monotonicity proof and semantic properties carry over unchanged — the state DAG loses two source nodes but gains no new transitions
- **Sharing preservation is the critical migration invariant**: thunk identity (`Rc<Thunk>` pointer) must be preserved through continuation dispatch. A materialized thunk must be the same allocation that was created at the definition site.
- MAX_EVAL_DEPTH should be removed; resource limits become configurable policy (`--max-depth`, `--max-memory`) rather than hardcoded safety bounds

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

#### Selective Materialization — Formal Specification

Specifies which arguments each Rust-native builtin forces (materializes) before execution and how the result is constructed. This is a two-tier specification: a **strictness signature table** covering all 44 builtins (auditable summary), plus **delta rules** for builtins whose forcing behavior cannot be captured by a flat per-argument annotation.

The signature notation draws on Mycroft's (1981) abstract interpretation framework for strictness analysis. The delta rules follow Plotkin's (1981) structural operational semantics, using the same judgment style as §Thunk Lifecycle — Formal Specification.

##### Part 1: Strictness Signature Notation

Each builtin receives a per-argument strictness annotation and a result classification:

**Input strictness (per argument position):**

| Symbol | Meaning | Implementation pattern |
|--------|---------|----------------------|
| `S` | Strict — argument is materialized before the builtin executes | `materialize(&args[i], None, depth)` |
| `L` | Lazy — argument passes through as a thunk; never materialized by this builtin | `Rc::clone(&args[i])` |
| `Sc` | Selectively strict — materialization is conditional on another argument's value; delta rule required | Pattern-match on a previously materialized value to decide |

**Result classification:**

| Symbol | Meaning | Description |
|--------|---------|-------------|
| `→ V` | Value result | Result is a fully computed atomic value (Int, Float, String, Bool) |
| `→ D` | Container result | Result is a Dict or Seq; values within may be thunks from inputs (structural preservation) |
| `→ Θ` | Thunk result | Result is a thunk (Rc::clone of an input, or a new PendingBuiltin/PendingCall) |
| `→ LT` | Lazy-transforming result | Result is a Dict or Seq containing *new* PendingCall/PendingBuiltin thunks wrapping inputs |
| `→ ⊥` | Divergent | Always raises an error; never returns a value |

For dual-dispatch builtins, the result classification refers to the more interesting path (typically Seq). Notes indicate when the Dict path differs.

**Derived property:** A builtin's category (§Stdlib Functions) can be approximated from its signature. These are sufficient conditions, not necessary-and-sufficient — builtins that materialize structure while preserving value thunks (e.g., `$merge`, `$collect`) span categories:
- **Structural** — all args are `L` and result preserves input thunks without new computation
- **Materializing** — all args are `S` and result contains no deferred computation from inputs
- **Lazy-transforming** — result is `→ LT` (contains new PendingCall/PendingBuiltin thunks)
- **Selective** — any arg is `Sc`

##### Part 2: Strictness Signature Table

All 44 Rust-native builtins. Builtins marked `†` have dual dispatch on Dict/Seq (delta rule required). Builtins marked `‡` have non-trivial forcing patterns (delta rule required).

**Arithmetic** (all materializing):

| Builtin | Signature | Category |
|---------|-----------|----------|
| `+` | `S × S → V` | Materializing |
| `-` | `S × S → V` | Materializing |
| `*` | `S × S → V` | Materializing |
| `/` | `S × S → V` | Materializing |

**Comparison** (all materializing):

| Builtin | Signature | Category |
|---------|-----------|----------|
| `=` | `S × S → V` | Materializing |
| `<` | `S × S → V` | Materializing |

**Control flow:**

| Builtin | Signature | Category | Notes |
|---------|-----------|----------|-------|
| `if` ‡ | `S × Sc × Sc → Θ` | Selective | Exactly one of args[1]/args[2] is forced; the other is never touched |

**Dict primitives:**

| Builtin | Signature | Category | Notes |
|---------|-----------|----------|-------|
| `keys` | `S → D` | Materializing | Materializes arg; returns Dict of key values (all newly constructed) |
| `length` | `S → V` | Materializing | Materializes arg; returns Int count |
| `merge` | `S × S → D` | Materializing | Materializes both dicts for key set; values are Rc::clone (thunks preserved) |
| `append` | `S × L → D` | Materializing | Materializes dict for key computation; value arg passes through as thunk |

**Strings** (all materializing):

| Builtin | Signature | Category |
|---------|-----------|----------|
| `str` | `S* → V` | Materializing (variadic) |
| `split` | `S × S → D` | Materializing |
| `replace` | `S × S × S → V` | Materializing |
| `upper` | `S → V` | Materializing |
| `lower` | `S → V` | Materializing |
| `trim` | `S → V` | Materializing |

**Numeric** (all materializing):

| Builtin | Signature | Category |
|---------|-----------|----------|
| `floor` | `S → V` | Materializing |
| `round` | `S → V` | Materializing |
| `to-int` | `S → V` | Materializing |
| `to-float` | `S → V` | Materializing |

**Evaluation control:**

| Builtin | Signature | Category | Notes |
|---------|-----------|----------|-------|
| `eval` | `S → V` | Materializing | Deep materialization — recursively forces all thunks |
| `error` | `S → ⊥` | Materializing | Always raises; never returns |
| `try` ‡ | `S → D` | Materializing | Materializes function arg, invokes it, catches errors |
| `apply` | `S × S → Θ` | Materializing | Materializes both; delegates to function invocation. Result type depends on the applied function |

**Type introspection:**

| Builtin | Signature | Category |
|---------|-----------|----------|
| `type-of` | `S → V` | Materializing |

**I/O:**

| Builtin | Signature | Category |
|---------|-----------|----------|
| `from-json` | `S → D` | Materializing |
| `include` | `S → D` | Materializing (I/O) |

**Sequence constructors:**

| Builtin | Signature | Category | Notes |
|---------|-----------|----------|-------|
| `seq` ‡ | `L × L → D` | Structural | Both args pass through as thunks inside the Seq value |
| `range` | `S (× S)? → LT` | Lazy-transforming | Materializes bounds; constructs Seq with PendingBuiltin tail |
| `repeat` | `L → LT` | Lazy-transforming | Arg passes through; PendingBuiltin tail for infinite repetition |
| `cycle` | `S → LT` | Lazy-transforming | Materializes dict; PendingBuiltin step for cycling |
| `iterate` ‡ | `L × L → LT` | Lazy-transforming | Both args pass through; PendingCall + PendingBuiltin for co-recursion |
| `unfold` | `L × L → Θ` | Lazy-transforming | Both args pass through; returns PendingBuiltin thunk |

**Sequence destructors:**

| Builtin | Signature | Category | Notes |
|---------|-----------|----------|-------|
| `head` ‡ | `S → Θ` | Structural | Materializes arg to verify Seq; returns head thunk (not forced) |
| `tail` ‡ | `S → Θ` | Structural | Materializes arg to verify Seq; returns tail thunk (not forced) |
| `collect` ‡ | `S → D` | Structural | Materializes Seq spine (all tails); head thunks pass through into Dict |
| `seq?` | `S → V` | Materializing | Materializes arg; returns Bool |

**Higher-order collection operations:**

| Builtin | Signature | Category | Notes |
|---------|-----------|----------|-------|
| `map` † ‡ | `L × S → LT` | Lazy-transforming | Function arg lazy; collection strict for dispatch |
| `filter` † ‡ | `L × S → LT` | Lazy-transforming | Predicate lazy at top level; collection strict for dispatch |
| `take` † | `S × S → LT` | Lazy-transforming | Both strict; Seq result has PendingBuiltin tail |
| `drop` † | `S × S → LT` | Lazy-transforming | Both strict; Seq result via PendingBuiltin step |
| `reduce` † ‡ | `L × L × S → LT` | Lazy-transforming | Function and init lazy; collection strict for dispatch |
| `join` † | `S × S → V` | Materializing | Both strict; materializes all elements for concatenation |

##### Part 3: Delta Rules

Delta rules specify the forcing behavior for builtins marked ‡ in the signature table, plus dual-dispatch builtins (†) whose Dict/Seq paths have materially different forcing patterns. Builtins marked † without ‡ (e.g., `$take`, `$drop`) follow the same dual-dispatch pattern as `$map`/`$filter` but with simpler per-path logic — their forcing is fully characterized by the signature.

Rules use the judgment form `δ(f, [θ₁, ..., θₙ], d, cs) ⇒ r` where f is the builtin, θᵢ are argument thunks, d is the current depth, cs is the call span, and r is the result (a thunk or error). All current delta rules use positional args only; named args are empty (`∅`) and omitted from rules for brevity.

**Depth in PendingBuiltin:** When constructing a PendingBuiltin, builtins that perform no materialization themselves (e.g., `$repeat`, `$iterate`, `$unfold`) store depth `0` because the stored depth is only used when the PendingBuiltin is eventually forced (the materialization-site depth governs recursive forcing via FORCE-BUILTIN in §Thunk Lifecycle). Builtins that materialize within the step function (e.g., `$filter` step, `$reduce` step) store the current `depth` for their internal materialization calls.

**[DELTA-IF-TRUE]**
```
force(θ_cond, d) ⇒ true
───────────────────────────
δ(if, [θ_cond, θ_then, θ_else], d, cs) ⇒ θ_then
```

**[DELTA-IF-FALSE]**
```
force(θ_cond, d) ⇒ false
───────────────────────────
δ(if, [θ_cond, θ_then, θ_else], d, cs) ⇒ θ_else
```

**Branch isolation guarantee:** The unchosen branch is never forced. `θ_then` and `θ_else` are returned via `Rc::clone` — no state transition occurs on the unchosen thunk. This is the foundational selective materialization property from which `$and`, `$or`, `$when`, `$unless`, and `$cond` derive their short-circuit behavior (see Part 5). The chosen branch thunk is returned to the caller; its subsequent forcing happens via FORCE-BUILTIN in §Thunk Lifecycle, which calls `force(θ', d+1)` on the builtin's result — the separation between "builtin execution" and "result forcing" is what makes `$if`'s laziness guarantee possible.

**[DELTA-SEQ]**
```
───────────────────────────
δ(seq, [θ_head, θ_tail], d, cs) ⇒ Materialized(Seq(Rc::clone(θ_head), Rc::clone(θ_tail)))
```

No arguments are forced. Both pass through as thunks within the Seq value. This is the coinductive guard — `$seq` enables corecursive definitions by deferring evaluation of both head and tail.

**[DELTA-HEAD]**
```
force(θ_xs, d) ⇒ Seq(θ_h, θ_t)
───────────────────────────
δ(head, [θ_xs], d, cs) ⇒ θ_h
```

**[DELTA-TAIL]**
```
force(θ_xs, d) ⇒ Seq(θ_h, θ_t)
───────────────────────────
δ(tail, [θ_xs], d, cs) ⇒ θ_t
```

DELTA-HEAD and DELTA-TAIL materialize the container to verify it is a Seq, but return the extracted thunk *without forcing it*. The head/tail thunk retains its original state (Unevaluated, PendingCall, etc.). Empty dict `[]` as input produces a specific error (`"head/tail on empty sequence"`).

**[DELTA-COLLECT-EMPTY]**
```
force(θ_xs, d) ⇒ Dict({})
───────────────────────────
δ(collect, [θ_xs], d, cs) ⇒ Materialized(Dict({}))
```

**[DELTA-COLLECT]**
```
force(θ_xs, d) ⇒ Seq(θ_h₁, θ_t₁)
force(θ_t₁, d) ⇒ Seq(θ_h₂, θ_t₂)
...
force(θ_tₙ, d) ⇒ Dict({})          (terminal)
───────────────────────────
δ(collect, [θ_xs], d, cs) ⇒ Materialized(Dict({0↦θ_h₁, 1↦θ_h₂, ..., n↦θ_hₙ}))
```

Collect materializes the Seq *spine* (all tail thunks) but head thunks pass through into the result Dict without forcing. This is the key distinction: `$collect` is strict in the structure but lazy in the values.

**[DELTA-ITERATE]**
```
───────────────────────────
δ(iterate, [θ_f, θ_x], d, cs) ⇒ Materialized(Seq(
    Rc::clone(θ_x),
    PendingBuiltin(iterate, [Rc::clone(θ_f), PendingCall(θ_f, [θ_x])], d, cs)
))
```

Fully lazy: neither f nor x is forced. The result Seq's head is x (unchanged thunk), and the tail is a PendingBuiltin that will produce `iterate(f, f(x))` when forced. The `f(x)` is itself a PendingCall — computation unfolds one step at a time. When the tail PendingBuiltin is forced, DELTA-ITERATE applies again with `f(x)` as the new seed, enabling corecursive unfolding of the infinite sequence.

**[DELTA-TRY]**
```
force(θ_func, d) ⇒ Function(params, body, env)    where |params| = 0
eval(body, env, d+1) ⇒ θ_body
force(θ_body, d+1) ⇒ v
───────────────────────────
δ(try, [θ_func], d, cs) ⇒ Materialized(Dict({"ok"↦Materialized(v)}))

force(θ_func, d) ⇒ Function(params, body, env)    where |params| = 0
eval(body, env, d+1) ⇒ θ_body
force(θ_body, d+1) ⇒ error(e)
───────────────────────────
δ(try, [θ_func], d, cs) ⇒ Materialized(Dict({"err"↦Materialized(e.message)}))
```

`$try` materializes the function argument and invokes it. On success, returns `[ok: value]`; on error, returns `[err: message]`. The error is caught — `$try` itself does not propagate errors (it is the catching boundary). Also handles Builtin callees (dispatches with zero args).

**[DELTA-MAP-DICT]**
```
force(θ_xs, d) ⇒ Dict({k₁↦θ₁, ..., kₙ↦θₙ})
∀i. θ'ᵢ = PendingCall(θ_f, [θᵢ], ∅, cs)
───────────────────────────
δ(map, [θ_f, θ_xs], d, cs) ⇒ Materialized(Dict({k₁↦θ'₁, ..., kₙ↦θ'ₙ}))
```

`θ_f` is never forced — it is captured by reference (`Rc::clone`) in each PendingCall. No values are computed; the result Dict is O(n) to construct and O(1) per element access.

**[DELTA-MAP-SEQ]**
```
force(θ_xs, d) ⇒ Seq(θ_h, θ_t)
θ'_h = PendingCall(θ_f, [θ_h], ∅, cs)
θ'_t = PendingBuiltin(map, [Rc::clone(θ_f), θ_t], ∅, d, cs)
───────────────────────────
δ(map, [θ_f, θ_xs], d, cs) ⇒ Materialized(Seq(θ'_h, θ'_t))
```

Recursive structure: head is a PendingCall, tail is a PendingBuiltin that will apply DELTA-MAP-DICT or DELTA-MAP-SEQ when forced.

**[DELTA-FILTER-DICT]**
```
force(θ_xs, d) ⇒ Dict({k₁↦θ₁, ..., kₙ↦θₙ})
θ_step = PendingBuiltin(filter_dict_step, [θ_pred, θ_xs_mat, θ_keys, θ_idx], ∅, d, cs)
    where θ_xs_mat, θ_keys, θ_idx are pre-computed materialized thunks
───────────────────────────
δ(filter, [θ_pred, θ_xs], d, cs) ⇒ θ_step
```

The predicate `θ_pred` is not forced at the top level — it is captured for deferred evaluation in the step function. The step function materializes one element at a time, applies the predicate, and either includes or skips it. Returns a Seq (not a Dict) because filtered keys are unpredictable.

**[DELTA-FILTER-SEQ]**
```
force(θ_xs, d) ⇒ Seq(_, _)
θ_step = PendingBuiltin(filter_seq_step, [θ_pred, θ_xs], d, cs)
───────────────────────────
δ(filter, [θ_pred, θ_xs], d, cs) ⇒ θ_step
```

The step function receives the *original seq thunk* (not destructured head/tail) and materializes it internally to obtain head and tail. This avoids redundant materialization since the dispatch already forced the collection. Lazy filter on sequences: the step function forces head, applies predicate, and either includes it (Seq node) or skips it (recurse on tail). Elements are tested only when the result Seq is consumed.

**[DELTA-REDUCE-DICT]**
```
force(θ_xs, d) ⇒ Dict({k₁↦θ₁, ..., kₙ↦θₙ})
acc₀ = θ_init
∀i. accᵢ = PendingCall(θ_f, [accᵢ₋₁, θᵢ], ∅, cs)
───────────────────────────
δ(reduce, [θ_f, θ_init, θ_xs], d, cs) ⇒ accₙ
```

Builds a chain of PendingCall thunks without forcing any values. The entire reduction is deferred — nothing computes until the result thunk is forced. At that point, the chain unwinds from the inside out.

**[DELTA-REDUCE-SEQ]**
```
force(θ_xs, d) ⇒ Seq(θ_h, θ_t)
θ_step = PendingBuiltin(reduce_seq_step, [θ_f, θ_init, θ_h, θ_t], ∅, d, cs)
───────────────────────────
δ(reduce, [θ_f, θ_init, θ_xs], d, cs) ⇒ θ_step
```

Seq reduction uses a step function that materializes the tail to check for termination, then recurses. Unlike Dict reduction, Seq reduction is incremental (processes one element per step function invocation).

##### Part 4: Dual-Dispatch Pattern

Six builtins (`map`, `filter`, `take`, `drop`, `reduce`, `join`) dispatch on the runtime type of their collection argument:

```
force(θ_xs, d) ⇒ v
    v = Dict(...)  →  apply Dict-specific rule
    v = Seq(...)   →  apply Seq-specific rule
    otherwise      →  type error
```

This dispatch materializes the collection argument to determine its type, then applies the appropriate delta rule. The function/predicate argument (if present) is *not* forced at dispatch time — it is captured by reference for deferred application.

**Result type asymmetry:** The Dict and Seq paths of a dual-dispatch builtin may produce different result types. For example, `$filter` on a Dict returns a Seq (not a Dict), because filtered keys are unpredictable. The signature table (Part 2) captures the Seq-path result; see §Type System and Dual-Dispatch Builtins for the full Dict-vs-Seq result matrix.

**CEK migration note:** Dual dispatch becomes a `Cont::CollectionDispatch` continuation that forces the collection, inspects its type, and pushes the appropriate next continuation. The function argument must be preserved on the continuation stack without forcing.

##### Part 5: Derived Selectivity

Standard library functions defined in `stdlib/prelude.llt` inherit their materialization behavior from the builtins they invoke. Key derived selectivity properties:

| Function | Definition | Inherited behavior |
|----------|------------|-------------------|
| `not` | `[fn [x] [call $if $x false true]]` | Materializing — forces x via `$if`'s condition position |
| `and` | `[fn [a b] [call $if $a $b false]]` | Selective — forces a; b forced only if a is true |
| `or` | `[fn [a b] [call $if $a true $b]]` | Selective — forces a; b forced only if a is false |
| `when` | `[fn [pred body] [call $if $pred $body []]]` | Selective — forces pred; body forced only if pred is true |
| `unless` | `[fn [pred body] [call $if $pred [] $body]]` | Selective — forces pred; body forced only if pred is false |
| `cond` | Recursive via `cond-impl` → `cond-check` → `$if` | Selective — forces conditions left-to-right via nested `$if`; first matching branch returned as thunk |
| `assert` | `[fn [cond msg] [call $if $cond true [call $error $msg]]]` | Selective — forces cond; error raised only if cond is false |

**Inheritance proof sketch:** Each derived function's selectivity follows by inlining its definition and applying DELTA-IF-TRUE/DELTA-IF-FALSE. For `$and`:

```
and(θ_a, θ_b)
  = if(θ_a, θ_b, false)
  DELTA-IF-TRUE:  force(θ_a) ⇒ true  → θ_b    (b is forced only when the caller forces the result)
  DELTA-IF-FALSE: force(θ_a) ⇒ false → false   (b is never touched)
```

This compositional guarantee means that making `$if` lazier (e.g., returning the branch as a thunk in Phase 5b — see §Current vs Planned Laziness Analysis) automatically improves all derived control flow functions without code changes.

##### Part 6: Properties and Guarantees

**Branch isolation (fundamental guarantee):**

For any builtin with `Sc` positions, the unchosen arguments are never forced, never transition state, and never appear in error traces. Formally: if `δ(if, [θ_c, θ_t, θ_e], d, cs) ⇒ θ_t`, then θ_e's `ThunkState` is unchanged after the call.

**No unnecessary forcing (structural guarantee):**

Builtins classified as Structural or Lazy-transforming force only the minimum arguments needed to determine the result structure. Value thunks within input collections pass through to output collections without forcing. This is verifiable by inspection: every `Rc::clone(&args[i])` or `Rc::clone(value_thunk)` preserves the thunk's state.

**Sharing preservation:**

All delta rules preserve thunk identity. When a thunk appears in both the input and output of a builtin (e.g., `$head` extracting a Seq's head), the same `Rc<Thunk>` allocation is shared — not copied. Subsequent forcing of the output thunk memoizes the value for all holders of that `Rc`.

**Strictness monotonicity:**

The signature table is monotonic with respect to the implementation: a builtin marked `L` at position i will never call `materialize()` on `args[i]`. A change that adds a `materialize()` call on a position marked `L` is a breaking change to the laziness contract and must update the signature table.

**Dual-dispatch consistency:**

For dual-dispatch builtins, the Dict and Seq paths must agree on which non-collection arguments are forced. For example, `$map`'s Dict path and Seq path both leave `θ_f` unforced — if one path started materializing `θ_f`, it would break laziness for programs that pass expensive computations as the function argument.

#### Call Convention — Formal Specification

Specifies how arguments at a call site are bound to function parameters. This is a dual-layer specification: **binding constraints** (declarative — what a valid binding is) and a **binding algorithm** (phased operational rules — how to compute it), connected by a **correctness proof** showing the algorithm computes the unique solution satisfying the constraints.

The constraint layer draws on Garrigue's (1995) treatment of labeled and optional arguments, which separates the binding environment for default evaluation from the closure environment. The phased algorithm follows the Kotlin/Scala model: any parameter is nameable at the call site, required and optional parameters may be freely interleaved in declarations, and the arity constraint is a per-parameter coverage check rather than a simple count (see C-COVERAGE below).

##### Notation

A function definition `[fn [p₁ p₂@[default: e₂] ...p₃] body]` has:

| Symbol | Meaning |
|--------|---------|
| `P = [p₁, ..., pₙ]` | Regular (non-variadic) parameters, ordered by position |
| `V` | Variadic parameter (if present): the `...name` param, always last |
| `required(pᵢ)` | `true` iff pᵢ has no `default:` annotation |
| `default(pᵢ)` | The default expression from pᵢ's `default:` annotation |
| `R = \|{pᵢ ∈ P \| required(pᵢ)}\|` | Count of required parameters |

A call site `[call $f a₁ a₂ k₁: v₁]` provides:

| Symbol | Meaning |
|--------|---------|
| `pos = [θ₁, ..., θₘ]` | Positional argument thunks, in order |
| `named = {k₁↦θ'₁, ..., kⱼ↦θ'ⱼ}` | Named argument thunks, keyed by name |
| `env_d` | Environment for evaluating default expressions |
| `env_c` | Closure environment (parent of the call environment) |

The environment parameter `env_d` is caller-controlled (Garrigue 1995): for normal calls, `env_d` is the caller's environment; for `$apply`, `env_d` is the closure environment (since `$apply` has no caller-side AST context for defaults).

##### Part 1: Binding Constraints (Declarative)

A **valid binding** for parameters `P`, optional variadic `V`, positional args `pos`, named args `named`, and default environment `env_d` is an environment `env_call` (with parent `env_c`) satisfying all of the following constraints simultaneously:

**[C-COVERAGE] Per-parameter coverage (Kotlin model):**

```
∀pᵢ ∈ P where required(pᵢ):  i < |pos|  ∨  pᵢ.name ∈ dom(named)
V = ∅ ⟹ |pos| ≤ |P|                         (no excess args without variadic)
```

Every required parameter must be covered by either a positional argument at its index or a named argument. This replaces a simple count-based arity check (`|pos| ≥ R`), which is insufficient when required parameters are interleaved with optional ones. Example: `[fn [a@[default: 1] b] body]` with one positional arg — count-based check passes (1 ≥ 1) but `b` at index 1 is unreachable.

**[C-PRIORITY] Binding priority chain:**

For each pᵢ ∈ P, exactly one case applies (in priority order):

```
(i)   i < |pos|                               ⟹  env_call(pᵢ) = pos[i]
(ii)  i ≥ |pos| ∧ pᵢ.name ∈ dom(named)       ⟹  env_call(pᵢ) = named[pᵢ.name]
(iii) i ≥ |pos| ∧ pᵢ.name ∉ dom(named)
      ∧ ¬required(pᵢ)                         ⟹  env_call(pᵢ) = eval(default(pᵢ), env_d)
```

If none of the three cases applies (i.e., i ≥ |pos|, not named, and required), C-COVERAGE is violated — no valid binding exists.

**[C-NO-OVERLAP] Positional/named exclusivity:**

```
∀(k, _) ∈ named:  ¬∃i < |pos| such that pᵢ.name = k
```

A named argument must not target a parameter already bound positionally.

**[C-NAMED-VALID] Named argument validity:**

```
∀(k, _) ∈ named:  ∃pᵢ ∈ P such that pᵢ.name = k
```

Named arguments may target any parameter (required or optional), but must target an existing parameter. This enables the Kotlin model: to reach a required parameter past an optional one, name it at the call site.

**[C-VARIADIC] Variadic collection:**

```
V ≠ ∅ ⟹ env_call(V) = Dict({k↦pos[|P|+k] | k ∈ 0..(|pos|-|P|)})
```

Excess positional arguments (beyond `|P|`) are collected into a Dict with integer keys starting at 0. If `|pos| = |P|`, the variadic Dict is empty (`{}`).

**[C-COMPLETE] Completeness:**

```
∀pᵢ ∈ P:  pᵢ.name ∈ dom(env_call)
V ≠ ∅ ⟹ V.name ∈ dom(env_call)
```

Every parameter receives a binding.

##### Part 2: Binding Algorithm (Phased Rules)

Five sequential phases compute the binding. The output of each phase flows into the next. The judgment form is `bind(P, V, pos, named, env_d, env_c) ⇒ env_call | error`.

**[BIND-SPLIT]**

```
params = [p₁, ..., pₙ]
    pₙ.variadic = true  →  P = [p₁, ..., pₙ₋₁],  V = pₙ
    otherwise            →  P = [p₁, ..., pₙ],     V = ∅
───────────────────────────
split(params) ⇒ (P, V)
```

The variadic parameter, if present, is always the last parameter. This is enforced by the parser.

**[BIND-ARITY]**

```
For each pᵢ ∈ P where required(pᵢ):
    if i ≥ |pos| ∧ pᵢ.name ∉ dom(named):
        error("missing argument for required parameter '{pᵢ.name}'")

V = ∅ ∧ |pos| > |P|         ⟹  error("arity mismatch: expected at most |P| arguments, got |pos|")
otherwise                    ⟹  pass
───────────────────────────
arity_check(P, V, pos, named) ⇒ pass | error
```

Per-parameter coverage check: each required parameter must be reachable via positional index or named argument. This handles interleaved required/optional parameters correctly — a required param at index 3 with an optional param at index 2 is valid if the required param is provided by name.

**[BIND-POSITIONAL]**

```
env₀ = Environment(parent: env_c)
For i = 0, ..., |P|-1:
    if i < |pos|:
        envᵢ₊₁ = envᵢ[pᵢ.name ↦ pos[i]]                          (positional arg)
    else if pᵢ.name ∈ dom(named):
        envᵢ₊₁ = envᵢ[pᵢ.name ↦ named[pᵢ.name]]                  (named arg fills gap)
    else if ¬required(pᵢ):
        envᵢ₊₁ = envᵢ[pᵢ.name ↦ eval(default(pᵢ), env_d, d+1)]  (default value)
    else:
        unreachable (BIND-ARITY guarantees every required pᵢ has i < |pos| ∨ pᵢ.name ∈ dom(named))
───────────────────────────
bind_positional(P, pos, named, env_d, env_c) ⇒ env_{|P|}
```

Parameters are bound left-to-right. For each parameter, the priority chain determines the source: positional arg first, then named arg, then default. This phase consumes named args that fill gaps beyond the positional args — BIND-NAMED handles only the unconsumed remainder.

The `env_d` parameter controls where default expressions are evaluated — this is the Garrigue (1995) separation. Defaults are evaluated eagerly at call time (not wrapped as thunks), so default-evaluation errors surface at the call site. This is consistent with Garrigue's system and with Kotlin/Scala.

**[BIND-NAMED]** (validation only)

```
For each (k, θ) ∈ named:
    if ∃i < |pos| such that pᵢ.name = k:
        error("parameter 'k' received both positional and named argument")
    if ¬∃pᵢ ∈ P such that pᵢ.name = k:
        error("unexpected named argument: k")
───────────────────────────
bind_named(P, pos, named, env_{|P|}) ⇒ env_{|P|} | error
```

BIND-NAMED is a pure validation phase — it performs no bindings. All named args that target valid parameters were already consumed by BIND-POSITIONAL (which checks `pᵢ.name ∈ dom(named)` for params past the positional args). After BIND-POSITIONAL, every param in P is bound in `env_{|P|}`. BIND-NAMED verifies two conditions: (1) overlap — no named arg targets a positionally-bound parameter, (2) existence — every named arg targets a parameter that exists. Named args may target any parameter (required or optional) — this is the Kotlin model.

The implementation may split this into two loops for engineering clarity (one for overlap, one for existence) without affecting semantics.

**[BIND-VARIADIC]**

```
V ≠ ∅:
    var_dict = Dict({k↦pos[|P|+k] | k ∈ 0..(|pos|-|P|)})
    env_call = env'[V.name ↦ Materialized(var_dict)]
V = ∅:
    env_call = env'
───────────────────────────
bind_variadic(V, P, pos, env') ⇒ env_call
```

The variadic parameter receives a Dict with integer keys. The Dict is materialized immediately (not a thunk) — the values within it are thunks from the positional args, preserving laziness of the individual arguments.

##### Part 3: Correctness Proof

**Theorem (Correctness of Binding Algorithm).** The phased binding algorithm (Part 2) computes the unique valid binding (Part 1) when one exists, and produces an error otherwise.

The proof has three parts: uniqueness of the declarative solution, soundness of the algorithm, and completeness.

**Uniqueness.** For each pᵢ ∈ P, the priority chain [C-PRIORITY] is deterministic: cases (i), (ii), (iii) are mutually exclusive because they partition the space by the condition `i < |pos|` and membership `pᵢ.name ∈ dom(named)`. Given fixed inputs, at most one case applies per parameter, so at most one environment satisfies all constraints simultaneously. The variadic binding [C-VARIADIC] is likewise deterministic (a fixed subsequence of `pos`). ∎

**Soundness.** Assume the algorithm produces `env_call` without error. Show each constraint holds:

- **C-COVERAGE:** BIND-ARITY explicitly checks per-parameter coverage for each required param and the upper bound. If the algorithm proceeds past BIND-ARITY, both conditions hold. ✓

- **C-PRIORITY:** BIND-POSITIONAL iterates over P in order. For each pᵢ:
  - If `i < |pos|`: binds `pos[i]` — matches case (i).
  - If `i ≥ |pos|` and `pᵢ.name ∈ dom(named)`: binds `named[pᵢ.name]` — matches case (ii).
  - If `i ≥ |pos|` and `pᵢ.name ∉ dom(named)` and `¬required(pᵢ)`: binds default — matches case (iii).
  - The else branch is unreachable: BIND-ARITY guarantees every required pᵢ has `i < |pos| ∨ pᵢ.name ∈ dom(named)`, so at least one of cases (i) or (ii) applies.

  Each case in the algorithm corresponds exactly to the matching constraint case. ✓

- **C-NO-OVERLAP:** BIND-NAMED checks `∃i < |pos| such that pᵢ.name = k` for each named arg and errors if true. If no error, the constraint holds. ✓

- **C-NAMED-VALID:** BIND-NAMED checks that each named arg targets an existing parameter. If no error, the constraint holds. ✓

- **C-VARIADIC:** BIND-VARIADIC constructs exactly the Dict specified by the constraint. ✓

- **C-COMPLETE:** BIND-POSITIONAL binds every pᵢ ∈ P (loop runs for all |P| params). BIND-VARIADIC binds V if present. ✓

All constraints satisfied. ∎

**Completeness.** Assume the constraints have a valid solution. Show the algorithm does not error:

- BIND-ARITY: C-COVERAGE guarantees every required pᵢ has `i < |pos| ∨ pᵢ.name ∈ dom(named)`, and `V = ∅ ⟹ |pos| ≤ |P|`. All checks pass.
- BIND-POSITIONAL: For each pᵢ where `i ≥ |pos|`: either `pᵢ.name ∈ dom(named)` (case ii of C-PRIORITY) or `¬required(pᵢ)` (case iii). The else branch is unreachable.
- BIND-NAMED overlap check: C-NO-OVERLAP guarantees no named arg targets a positionally-bound param.
- BIND-NAMED existence check: C-NAMED-VALID guarantees all named args target existing params.
- BIND-VARIADIC: No error conditions.

No error is produced. ∎

**Corollary (Unique binding).** Since the solution is unique and the algorithm computes it, `bind_args_thunks` produces the unique valid binding for any given call. There are no alternative valid bindings that the algorithm might miss.

##### Part 4: Error Taxonomy

The binding algorithm produces four distinct error classes. Each corresponds to a constraint violation:

| Error | Constraint violated | Message pattern | Source |
|-------|-------------------|-----------------|--------|
| Uncovered required param | C-COVERAGE | `"missing argument for required parameter '{pᵢ.name}'"` | BIND-ARITY |
| Too many args | C-COVERAGE (upper) | `"arity mismatch: expected at most {|P|} arguments, got {|pos|}"` | BIND-ARITY |
| Positional/named overlap | C-NO-OVERLAP | `"parameter '{k}' received both positional and named argument"` | BIND-NAMED |
| Nonexistent named arg | C-NAMED-VALID | `"unexpected named argument: {k}"` | BIND-NAMED |

Default evaluation errors (from `eval(default(pᵢ), env_d)` in BIND-POSITIONAL) are not binding errors — they propagate as normal evaluation errors with the default expression's span.

**Implementation note:** The current implementation (eval.rs:520-626) uses a count-based arity check and restricts named args to `default:` params. Adopting the Kotlin model requires two implementation changes: (1) replace the count check with per-parameter coverage, (2) remove the `get_default(p).is_some()` guard in the named-arg validity check. The spec documents the target semantics.

##### Part 5: `$apply` and the Default Environment

The `default_env` parameter is the key difference between normal calls and `$apply`:

```
eval_call:     default_env = caller's environment (env)
$apply:        default_env = closure environment  (env_c)
```

**Why `$apply` uses `env_c`:** `$apply` receives a function value and a dict of arguments at runtime — there is no caller-side AST context. Default expressions reference names from the function's definition site, not the call site. Using the closure environment ensures defaults resolve correctly.

**Formal consequence:** The binding constraints [C-PRIORITY case (iii)] use `eval(default(pᵢ), env_d)`. The environment `env_d` is a parameter of the judgment, not fixed. This makes the specification parametric over the default evaluation strategy — both `eval_call` and `$apply` are instances of the same binding algorithm with different `env_d` values.

**Correctness is preserved:** The correctness proof (Part 3) is parametric in `env_d`. Changing `env_d` affects which values defaults evaluate to, but not the structure of the binding (which params get positional vs named vs default). Soundness, completeness, and uniqueness hold for any `env_d`.

**Variadic typing precision:** The type checker assigns variadic parameters type `Record([], Closed)` regardless of actual arguments (§Type Inference Algorithm, Limitation #8). The runtime Dict has integer-keyed entries with the excess args' types. A precise type would require dependent types (the length depends on `|pos| - |P|`). The current typing is a sound over-approximation — accessing variadic fields produces type errors that succeed at runtime. See Limitation #8 for the planned fix (`Record([], Open)` or `Any`).

**PendingCall interaction:** When a `PendingCall` thunk is forced, it invokes `invoke_function`, which calls `bind_args_thunks` — the same binding algorithm specified above. The forcing semantics (state transitions, memoization, error handling) are specified in §Thunk Lifecycle — Formal Specification, rules FORCE-CALL and FORCE-CALL-BUILTIN.

##### Part 6: Worked Example

Trace all five phases for a call with interleaved required/optional parameters:

```lisp
greet: [fn [greeting@[default: "hello"] name sep@[default: " "]]
    [call $str $greeting $sep $name]]

[call $greet name: "Alice"]
```

**BIND-SPLIT:** `params = [greeting, name, sep]`. No variadic.
- `P = [greeting, name, sep]`, `V = ∅`

**BIND-ARITY:** Required params = `{name (index 1)}`.
- `name`: `1 < |pos|`? No (`|pos| = 0`). `"name" ∈ dom(named)`? Yes. ✓ Covered.
- Upper bound: `|pos| = 0 ≤ |P| = 3`. ✓

**BIND-POSITIONAL:** `pos = []`, `named = {"name"↦θ_Alice}`.

| i | param | `i < \|pos\|`? | `name ∈ dom(named)`? | `¬required`? | Binding |
|---|-------|-----------|------------------|------------|---------|
| 0 | `greeting` | No (0 < 0) | No | Yes | `eval("hello", env_d)` → `"hello"` |
| 1 | `name` | No (1 < 0) | Yes | — | `named["name"]` → `θ_Alice` |
| 2 | `sep` | No (2 < 0) | No | Yes | `eval(" ", env_d)` → `" "` |

Result: `env₃ = {greeting↦"hello", name↦θ_Alice, sep↦" "}`

**BIND-NAMED:** Validate named args.
- `("name", θ_Alice)`: overlap? `∃i < 0` with `pᵢ.name = "name"`? No. ✓ Exists? `name ∈ P`? Yes. ✓

**BIND-VARIADIC:** `V = ∅`, skip.

**Result:** `env_call = {greeting↦"hello", name↦θ_Alice, sep↦" "}`. Evaluates to `"hello Alice"`.

Without the Kotlin model, this call would fail — `name` has no `default:`, so it couldn't be named. The caller would have to write `[call $greet "hello" "Alice"]`, defeating the purpose of `greeting`'s default.

**`PendingCall` thunk state:**

To make dict-returning operations lazy, the thunk model gains a new state:

```
PendingCall(function: Rc<Thunk>, args: Vec<Rc<Thunk>>, call_span: Span)
```

`PendingCall` represents "apply this function to these arguments when forced." It enables lazy function application at runtime without constructing AST nodes. When a `PendingCall` thunk is materialized, it calls the function and memoizes the result (transitioning to `Materialized`), just like `PendingBuiltin` does for builtin calls.

This is different from `PendingBuiltin` in a key way:
- **PendingBuiltin** stores a Rust function pointer (`BuiltinFn`) and its arguments — the builtin runs when materialized
- **PendingCall** stores a user-defined function thunk, its argument thunks, and a `call_span: Span` (for error reporting) — invokes `invoke_function()` when materialized

Both support lazy evaluation, but `PendingCall` works at the LLT function level (no AST needed), while `PendingBuiltin` works at the Rust builtin level.

**Type transparency:** `PendingCall` is invisible to the type system — a `PendingCall(f, [x])` has the same inferred type as `f(x)`. No new `Type` variant is needed; HM type inference is unchanged.

**Error reporting:** When `PendingCall` materialization fails, the definition-site span comes from the function's body, the materialization-site span from where the thunk was forced, and a stack frame is added with the deferred call's creation span (from `call_span`).

**Motivation:** Operations like `$map` on dicts need to create new thunks that apply a function to each value, but they can't store AST nodes (the function comes from a runtime variable). `PendingCall` lets them defer function application without needing to construct new AST `CallExpr` nodes.

#### Equality and Comparison — Formal Specification

This section formalizes the two primitive comparison builtins (`$=` and `$<`) and the three derived comparison operators (`$>`, `$<=`, `$>=`). The specification defines type-dispatch semantics, totality and partiality properties, cross-type promotion, and the algebraic properties these relations satisfy or intentionally violate.

##### Part 1: Primitive Relations

Two builtins form the comparison basis. All others are derived compositions.

**EQ — Total equality (`$=`):**

```
EQ(θ₁, θ₂, d, s) :
  v₁ = materialize(θ₁, _, d)
  v₂ = materialize(θ₂, _, d)
  ─────────────────────────────
  ⟨v₁, v₂⟩ ↦ Bool(dispatch_eq(v₁, v₂))
```

**LT — Partial ordering (`$<`):**

```
LT(θ₁, θ₂, d, s) :
  v₁ = materialize(θ₁, _, d)
  v₂ = materialize(θ₂, _, d)
  ─────────────────────────────
  ⟨v₁, v₂⟩ ↦ Bool(dispatch_lt(v₁, v₂))    if defined
  ⟨v₁, v₂⟩ ↦ Error(type_mismatch, s)       otherwise
```

The `_` in `materialize(θ, _, d)` is the materialization span (`Option<&Span>`), passed as `None` by both builtins — it is a diagnostic concern, not a semantic parameter. The span `s` is the call-site span: unused in EQ (total function, never errors) but required for LT error reporting.

Both builtins require exactly 2 positional arguments and reject named arguments (`reject_named`). Both are **inherently materializing**: they must inspect the concrete values of both operands to produce a result. This is a §Selective Materialization boundary — comparison always forces. If materialization of either operand raises an error (cycle detection, division by zero, depth limit), that error propagates immediately — comparison dispatch is never reached.

##### Part 2: Type-Dispatch Tables

**`dispatch_eq(v₁, v₂) → bool`:**

| v₁ | v₂ | Result | Rule |
|----|----|--------|------|
| Int(a) | Int(b) | a == b | EQ-INT |
| Float(a) | Float(b) | a == b (IEEE 754) | EQ-FLOAT |
| String(a) | String(b) | a == b (byte equality) | EQ-STR |
| Bool(a) | Bool(b) | a == b | EQ-BOOL |
| Int(a) | Float(b) | (a as f64) == b | EQ-PROMOTE-IF |
| Float(a) | Int(b) | a == (b as f64) | EQ-PROMOTE-FI |
| _ | _ | false | EQ-INCOMP |

**`dispatch_lt(v₁, v₂) → bool | ⊥`:**

| v₁ | v₂ | Result | Rule |
|----|----|--------|------|
| Int(a) | Int(b) | a < b | LT-INT |
| Float(a) | Float(b) | a < b (IEEE 754) | LT-FLOAT |
| String(a) | String(b) | a < b (lexicographic) | LT-STR |
| Bool(a) | Bool(b) | ¬a ∧ b (false < true) | LT-BOOL |
| Int(a) | Float(b) | (a as f64) < b | LT-PROMOTE-IF |
| Float(a) | Int(b) | a < (b as f64) | LT-PROMOTE-FI |
| _ | _ | ⊥ (type error) | LT-ERROR |

The critical difference: EQ-INCOMP returns `false` (totality), while LT-ERROR raises a type error (partiality). This reflects the design that "are these equal?" always has a reasonable answer (no, different types are never equal), while "is this less than that?" has no meaningful answer across incompatible types.

##### Part 3: Cross-Type Promotion Semantics

Int/Float promotion uses Rust's `as f64` cast, which is the IEEE 754 `convertToFloat64` operation. This is exact for integers in the range [−2⁵³, 2⁵³] but loses precision outside it:

```
Promotion: Int(n) → Float(n as f64)

Exact range:  |n| ≤ 2⁵³ (9,007,199,254,740,992)
Loss example: Int(2⁵³ + 1) promotes to Float(2⁵³)
              → EQ-PROMOTE: [call $= 9007199254740993 9007199254740992.0] = true  (!)
```

**Design rationale:** The alternative — rejecting cross-type comparison entirely — would force users to manually cast in every mixed expression. The promotion follows JavaScript, Python, Ruby, and Lua conventions. The precision-loss edge case affects only integers outside the safe range, which is rare in configuration contexts.

Promotion is **symmetric**: `EQ-PROMOTE-IF` and `EQ-PROMOTE-FI` always produce the same result because IEEE 754 `==` is symmetric and `as f64` is deterministic.

##### Part 4: Derived Relations

Three comparison operators are derived from `$<` and `$not` in `stdlib/prelude.llt`:

```
GT(a, b)  ≡  LT(b, a)               # >:  [fn [a b] [call $< $b $a]]
LEQ(a, b) ≡  ¬LT(b, a)              # <=: [fn [a b] [call $not [call $< $b $a]]]
GEQ(a, b) ≡  ¬LT(a, b)              # >=: [fn [a b] [call $not [call $< $a $b]]]
```

Note: `$<=` is defined as `¬GT` (not as `LT ∨ EQ`), and `$>=` as `¬LT` (not as `GT ∨ EQ`). These are equivalent for total orders but diverge in the presence of NaN (see Part 5). The stdlib definitions are correct because `$<` is a strict weak order on each comparable type (NaN is incomparable to everything, and `¬(NaN < x)` correctly yields `true` for `$>=`... but see the NaN anomaly below).

##### Part 5: IEEE 754 NaN Behavior

Float comparison follows IEEE 754 semantics inherited from Rust's `f64` operations:

```
EQ-FLOAT with NaN:   NaN == NaN → false     (IEEE 754 §5.11)
LT-FLOAT with NaN:   NaN < x   → false      (for any x, including NaN)
                      x < NaN   → false      (for any x, including NaN)
```

**Consequence for derived relations:**

```
[call $=  NaN NaN]  → false    (NaN ≠ NaN — correct per IEEE 754)
[call $<  NaN 1.0]  → false    (NaN is unordered)
[call $>  NaN 1.0]  → false    (= $< 1.0 NaN → false)
[call $<= NaN 1.0]  → true     (= $not [$< 1.0 NaN] = $not false = true — ANOMALY)
[call $>= NaN 1.0]  → true     (= $not [$< NaN 1.0] = $not false = true — ANOMALY)
```

The `$<=` and `$>=` anomalies arise because the stdlib derives them via negation of the *swapped* `$<`, rather than via `LT ∨ EQ`. Under IEEE 754, `¬(b < a)` is *not* equivalent to `a ≤ b` when either operand is NaN. This is a known deviation: IEEE 754 §5.11 defines `totalOrder` separately from the partial comparison predicates.

**NaN-vs-NaN anomaly:**

```
[call $<= NaN NaN]  → true     (= $not [$< NaN NaN] = $not false = true)
[call $>= NaN NaN]  → true     (= $not [$< NaN NaN] = $not false = true)
```

Both `$<= NaN NaN` and `$>= NaN NaN` return `true`, even though `$= NaN NaN` returns `false`. Tinct reports NaN as both "less-than-or-equal-to itself" and "greater-than-or-equal-to itself" while simultaneously reporting it as "not equal to itself."

**NaN/Infinity rejection (decided):** Tinct enforces the invariant that **all floats are finite**. Non-finite values are rejected at two layers: (1) `$from-json` rejects `f64::INFINITY` and `f64::NAN` from `serde_json::Number::as_f64()` at parse time, closing the entry path, and (2) arithmetic builtins (`$+`, `$-`, `$*`, `$/`) reject non-finite results via a shared `check_float_result` helper, catching overflow (`1e308 + 1e308`) at point of origin. This matches the consensus approach for config languages targeting JSON output (Jsonnet, Nickel, CUE all reject non-finite floats). With this invariant, the `$<=`/`$>=` NaN anomaly documented above becomes unreachable — it is retained as documentation of IEEE 754 behavior but cannot occur in practice.

**Pragmatic justification for the anomaly documentation:** The `$<=`/`$>=` NaN anomaly is documented but not fixed (no `$is-nan` check in derived comparisons) because the finite-float invariant makes it unreachable. If the invariant were ever relaxed, the negation-based derivation would need revisiting.

##### Part 6: Key Ordering (`Key::PartialOrd`)

Separate from value comparison, the `Key` type has its own partial ordering used by range access (`$data[start..end]`):

```
Key::partial_cmp:
  (Int(a),    Int(b))    → Some(a.cmp(b))     # total within Int keys
  (String(a), String(b)) → Some(a.cmp(b))     # total within String keys (lexicographic)
  (Int(_),    String(_)) → None                # mixed key types: incomparable
  (String(_), Int(_))    → None                # mixed key types: incomparable
```

Mixed-type key comparison in range access raises an error (via `key_in_range`, §Access Chain Evaluation). `Key::PartialOrd` is semantically equivalent to the Int/String subset of `dispatch_lt` but exists as a separate relation because it operates at the `Key` level (before value materialization), while `$<` operates at the `Value` level (after materialization). Range access needs to filter dict keys without forcing any values — it compares keys directly from the `IndexMap<Key, Rc<Thunk>>`, never touching the thunks. This is an optimization that preserves laziness: `$data[2..5]` filters keys without materializing any values.

##### Part 7: `Value::PartialEq` vs `$=` Divergence

The Rust-level `Value::PartialEq` implementation differs from the `$=` builtin:

| Aspect | `Value::PartialEq` | `$=` builtin |
|--------|-------------------|-------------|
| Int/Float cross-type | `false` (different variants) | promotes Int → Float |
| Dict, Function, Builtin, Seq | `false` (catch-all) | `false` (catch-all) |
| NaN == NaN | `false` (IEEE 754 via `f64::eq`) | `false` (IEEE 754 via `f64::eq`) |
| Used by | Internal Rust code, tests | User-facing tinct programs |

The divergence is intentional: `Value::PartialEq` uses Rust's native dispatch (no cross-variant matching), while `$=` adds the Int/Float promotion rules (EQ-PROMOTE-IF, EQ-PROMOTE-FI) that users expect from a dynamically-typed numeric tower. Internal Rust code must use `Value::PartialEq` for exact variant matching (tests, pattern matching). User-facing tinct programs must use `$=` via the builtin. Never compare `Value` instances directly in user-facing contexts — the missing cross-type promotion would silently give wrong answers for mixed Int/Float comparisons.

##### Part 8: Properties

**P1 — EQ reflexivity (conditional):** `∀v. dispatch_eq(v, v) = true` **iff** `v ∉ {NaN, Dict, Function, Builtin, Seq}`. NaN violates reflexivity per IEEE 754. Dict/Function/Builtin/Seq return false even for identity (same Rc pointer) because no structural comparison is performed — structural dict equality would violate lazy evaluation by forcing all field thunks (e.g., comparing `[x: [call $/ 1 0]]` with itself would force the division-by-zero error in an unreferenced field). **Future breaking change:** if typeclasses add user-defined equality, `[call $= [x: 1] [x: 1]]` would change from `false` to `true`. Current code relying on dicts always being unequal may break.

**P2 — EQ symmetry:** `∀v₁, v₂. dispatch_eq(v₁, v₂) = dispatch_eq(v₂, v₁)`. Holds unconditionally — the dispatch table is symmetric (EQ-PROMOTE-IF and EQ-PROMOTE-FI produce identical results; EQ-INCOMP is symmetric; IEEE 754 `==` is symmetric).

**P3 — EQ transitivity (conditional):** `dispatch_eq(a, b) ∧ dispatch_eq(b, c) → dispatch_eq(a, c)` holds within each type. **WARNING: Cross-type promotion violates transitivity at the 2⁵³ boundary.** Concrete example: `dispatch_eq(Int(2⁵³+1), Float(2⁵³)) = true` (EQ-PROMOTE-IF, both promote to same float) and `dispatch_eq(Float(2⁵³), Int(2⁵³)) = true` (EQ-PROMOTE-FI), but `dispatch_eq(Int(2⁵³+1), Int(2⁵³)) = false` (EQ-INT, distinct integers). Programs relying on equivalence substitution for integers outside [−2⁵³, 2⁵³] will observe non-transitive equality.

**P4 — LT irreflexivity:** `∀v. dispatch_lt(v, v) = false` wherever defined. Holds for Int, Float (excluding NaN, which returns false for `<` anyway), String, Bool. NaN: `NaN < NaN → false` — technically satisfies irreflexivity even though NaN is unordered.

**P5 — LT asymmetry:** `dispatch_lt(a, b) = true → dispatch_lt(b, a) = false`. Holds for all comparable types. (Consequence: `dispatch_lt(a, b) ∧ dispatch_lt(b, a)` is impossible.)

**P6 — LT transitivity:** `dispatch_lt(a, b) ∧ dispatch_lt(b, c) → dispatch_lt(a, c)` within each type. Cross-type Int/Float promotion inherits the same precision-boundary caveat as EQ transitivity (P3).

**P7 — LT/EQ trichotomy (conditional):** Trichotomy holds within each type (excluding NaN): exactly one of `dispatch_lt(a, b)`, `dispatch_eq(a, b)`, `dispatch_lt(b, a)` is true. Two violations: (1) NaN — all three are false; (2) cross-type Int/Float at the precision boundary — promotion may cause both `dispatch_lt` and `dispatch_eq` to disagree with same-type comparisons (same caveat as P3).

**P8 — Totality of EQ:** `$=` never errors. For any two values (including incompatible types), it returns a Bool. This is the defining characteristic that distinguishes it from `$<`.

**P9 — Partiality of LT:** `$<` errors on type pairs not in the dispatch table (LT-ERROR). The comparable domain is: {Int, Float} × {Int, Float} ∪ String × String ∪ Bool × Bool.

**P10 — Materialization obligation:** Both `$=` and `$<` call `materialize(θ, _, d)` on both arguments before dispatch. This is a forcing operation (§Thunk Lifecycle: FORCE-EVAL, FORCE-BUILTIN, or FORCE-CALL depending on the thunk's state) — the thunk moves from Unevaluated/PendingCall/PendingBuiltin to Evaluated, and the resulting value is cached for subsequent access. If materialization detects a cycle (thunk in InProgress state), it raises a circular dependency error via FORCE-CYCLE — comparison dispatch is never reached. Note: for Dict/Seq values, `$=` materializes the outer structure (forces the thunk to produce a `Value::Dict` or `Value::Seq`) but does NOT recursively force field values — it matches on the Value variant and returns `false` (EQ-INCOMP) immediately.

#### Merge — Formal Specification

This section formalizes `$merge`, the only builtin that allows key collision. The specification defines operational semantics (right-biased merge with insertion-order preservation), algebraic properties, interaction with record typing (closed records now, forward-compatible with row variables), and the lazy overlay compatibility invariant for Phase 5b.

`$merge` is the composition primitive: it underlies shared base config (`$merge $base $overrides`), `$set` (single-key overlay), `$from-entries` (construction from pairs), and `$map` on dicts (per-entry rebuild). Its semantics propagate through these dependents.

##### Part 1: Notation

| Symbol | Meaning |
|--------|---------|
| `D = {k₁↦θ₁, ..., kₙ↦θₙ}` | A dict: ordered map from keys to thunks |
| `K(D)` | Key set of D: `{k₁, ..., kₙ}` |
| `D(k)` | Thunk bound to key k in D |
| `\|D\|` | Number of entries in D |
| `pos(D, k)` | Insertion-order position of key k in D (0-indexed) |
| `∅` | Empty dict `{}` |
| `θ` | A thunk (§Thunk Lifecycle) — values remain unevaluated |

Keys are materialized values (`Key` type: Int, String). Values are thunks — `$merge` never materializes values, only dict structure.

##### Part 2: Operational Rule

**[MERGE]**

```
materialize(θ_L, _, d) ⇒ Dict(L)
materialize(θ_R, _, d) ⇒ Dict(R)
Result = L ⊕ R
───────────────────────────
merge(θ_L, θ_R, d, s) ⇒ ok_val(Dict(Result))
```

where `L ⊕ R` (right-biased merge) is defined as:

```
L ⊕ R = D  where
  dom(D) = K(L) ∪ K(R)
  D(k) = R(k)           if k ∈ K(R)         [RIGHT-BIAS]
  D(k) = L(k)           if k ∈ K(L) \ K(R)  [LEFT-KEEP]
```

**Iteration order of D:**

```
order(D) = order_L(L, R) ++ new(R, L)  where
  order_L(L, R) = [k for k in L in insertion order]
                  (values replaced by R(k) where k ∈ K(R), position unchanged)
  new(R, L)     = [k for k in R in insertion order where k ∉ K(L)]
```

Left keys retain their positions. Right keys that collide replace the value at the left key's position. Right keys that are new are appended in their original order.

**Strictness (current):** `S × S → D` (§Selective Materialization). Both arguments are materialized to inspect dict structure (key sets). Values are `Rc::clone` (thunk pointers copied, not forced). This is a structural materialization — it forces the thunks to produce `Value::Dict` but does not recurse into field values. Phase 5b changes strictness to `L × L → D` — the overlay defers both operands' materialization until access (see Part 5).

When both operands are list-dicts (integer keys `0..n`), `$merge` performs positional override, not concatenation: `merge([a b c], [x y])` produces `{0:x, 1:y, 2:c}`. Use `$concat` for list concatenation.

**Error cases:**

| Condition | Error |
|-----------|-------|
| `args.len() ≠ 2` | Arity mismatch |
| `materialize(θ_L) ⇒ v` where v is not Dict | Type error: "merge: expected Dict, got {type}" |
| `materialize(θ_R) ⇒ v` where v is not Dict | Type error: "merge: expected Dict, got {type}" |
| `materialize(θ_L)` or `materialize(θ_R)` raises error | Error propagates (cycle, depth limit) |

Named arguments are rejected (`reject_named`).

##### Part 3: Typing Rules

**Current state:** `$merge` is typed as `Any → Any → Any`. The rules below specify the target type for the type-extensions roadmap Phase 1 (§Type System Extension Roadmap) when `TypeEnv::with_builtins()` registers precise builtin signatures. When an operand has type `TypeVar(α)`, Phase 1 falls back to T-MERGE-ANY (treating unresolved type variables as `Any`). After row-variable unification, option (a) — unifying `α` with a fresh open record type — becomes available but is not required for Phase 1.

**[T-MERGE] Closed records:**

T-MERGE applies only when both operands have closed record types (`RowRest::Closed`). Open records (`RowRest::Open` or `RowRest::RowVar`) fall through to T-MERGE-ANY until row-variable unification is implemented.

```
Γ ⊢ L : Record(F_L, Closed),  Γ ⊢ R : Record(F_R, Closed)
───────────────────────────────────────────────────────────
Γ ⊢ merge(L, R) : Record(F_L ⊕ F_R, Closed)
```

where `F_L ⊕ F_R` is the field-level right-biased merge:

```
dom(F_L ⊕ F_R) = dom(F_L) ∪ dom(F_R)
(F_L ⊕ F_R)(k) = F_R(k)     if k ∈ dom(F_R)          [T-RIGHT-BIAS]
(F_L ⊕ F_R)(k) = F_L(k)     if k ∈ dom(F_L) \ dom(F_R) [T-LEFT-KEEP]
```

For shared keys, the right operand's type wins. This mirrors the runtime semantics: the right value is what gets returned on access.

**[T-MERGE-ANY] Gradual fallback:**

```
Γ ⊢ L : Any   or   Γ ⊢ R : Any
────────────────────────────────
Γ ⊢ merge(L, R) : Any
```

If either operand has type `Any` (unannotated, forward reference, or gradual escape), the result is `Any`. The type checker cannot compute field-level merge without knowing the field sets. This also applies when an operand is a `TypeVar` (Phase 1) or has an open record type (pre-row-unification).

**Design choice:** When only one operand is `Any`, partial information could be preserved (e.g., `merge(Any, Record(F, Closed)) : Record(F, Open)`). This is rejected: it complicates the gradual typing story (§Type System Extension Roadmap, Phase 3) and gains little in practice. Deferred to Phase 3 gradual typing formalization.

**Forward compatibility with row variables:** When row-variable unification (§Row-Variable Unification — Kinded Rémy Model) is implemented, the typing rule generalizes to:

```
Γ ⊢ L : Record(F_L, ρ₁),  Γ ⊢ R : Record(F_R, ρ₂)
─────────────────────────────────────────────────────
Γ ⊢ merge(L, R) : Record(F_L ⊕ F_R, ρ₃)
```

where `ρ₃` captures fields from `ρ₁` and `ρ₂` not in the known field sets. The precise definition of `ρ₃` depends on the row-unification design — Harper & Pierce (1991) require disjointness (`K(ρ₁) ∩ K(ρ₂) = ∅`) for symmetric concatenation, but tinct's right-biased semantics relax this. Rémy (1994) handles non-disjoint row extensions via presence/absence flags; tinct's right-bias is a simpler alternative that achieves similar expressiveness without the full flag system.

The row-unification sprint must define how `⊕` interacts with row tails, subject to three constraints:

1. **Closed-record preservation:** When `ρ₁ = ρ₂ = ∅`, T-MERGE (closed records) is recovered as a special case.
2. **Common-tail preservation:** When `ρ₁ = ρ₂ = ρ`, then `ρ₃ = ρ` — merge preserves the common tail because it neither adds nor removes fields from the unknown extension.
3. **Principality:** The choice of `ρ₃` must preserve principal types. When `ρ₁ ≠ ρ₂`, options include: (a) fresh `ρ₃` constrained by `ρ₁` and `ρ₂`, (b) unify `ρ₁` and `ρ₂`, or (c) error on incompatible open records. See §Row-Variable Unification Case 4 (fresh row variable for shared unknown tail) for the pattern.

##### Part 4: Algebraic Properties

**P1 — Right-bias identity:** `(L ⊕ R)(k) = R(k)` for all `k ∈ K(R)`. The right operand's value is always chosen for shared keys, regardless of the left operand's value. This is the defining property.

**P2 — Left identity:** `∅ ⊕ R = R`. Merging an empty dict on the left produces the right dict unchanged. Both key set and iteration order are preserved.

**P3 — Right identity:** `L ⊕ ∅ = L`. Merging an empty dict on the right produces the left dict unchanged.

**P4 — Associativity (content and iteration order):** `(A ⊕ B) ⊕ C = A ⊕ (B ⊕ C)` on both key-value content and iteration order. The rightmost dict wins for any key: in both groupings, `C(k)` wins if `k ∈ K(C)`, else `B(k)` if `k ∈ K(B)`, else `A(k)`.

Iteration-order proof: In `L ⊕ R`, the result order is `[keys from L in L's order] ++ [keys from R not in L, in R's order]`. For the three-operand case, both groupings produce `[A keys] ++ [B keys \ A] ++ [C keys \ (A ∪ B)]`, each segment preserving its source's insertion order. This follows from IndexMap's insert-at-existing-position semantics: the leftmost operand containing a key determines its position.

**P5 — Non-commutativity:** `L ⊕ R ≠ R ⊕ L` in general. Counterexample: `{x↦1} ⊕ {x↦2} = {x↦2}`, but `{x↦2} ⊕ {x↦1} = {x↦1}`. Right-bias makes merge inherently directional.

**P6 — Idempotence:** `D ⊕ D = D`. Merging a dict with itself produces the same dict (same keys, same thunks — `Rc::clone` of the same allocation).

**P7 — Monoid structure:** `(Dict, ⊕, ∅)` forms a monoid over ordered maps: ⊕ is associative on both content and iteration order (P4) with identity element ∅ (P2, P3). It is not a commutative monoid (P5). This justifies n-ary merge as a left fold: `merge*(D₁, ..., Dₙ) = (...((D₁ ⊕ D₂) ⊕ D₃)... ⊕ Dₙ)`, where later operands take priority. By P4, any grouping produces the same result.

**P8 — Value preservation:** `$merge` never materializes, transforms, or copies values. It copies thunk pointers (`Rc::clone`). After `D = L ⊕ R`, for any key k, `D(k)` is the exact same `Rc<Thunk>` as `R(k)` or `L(k)` — not a new thunk wrapping the old one. This preserves sharing (§Thunk Lifecycle: evaluate-at-most-once).

##### Part 5: Lazy Overlay Compatibility

Phase 5b (§Current vs Planned Laziness Analysis) replaces eager cloning with a lazy overlay representation. The overlay defers the merge operation itself:

```
Overlay(L, R) — O(1) construction
  access(k): if k ∈ K(R) then R(k) else L(k) — O(1) per key
  iterate:   flatten to concrete IndexMap — O(|L| + |R|)
```

The lazy overlay must satisfy **behavioral equivalence**: for any program P, replacing the eager `L ⊕ R` with `Overlay(L, R)` produces the same observable results (modulo documented error timing differences). Specifically:

1. **Same values:** `Overlay(L, R)(k) = (L ⊕ R)(k)` for all `k ∈ dom(L) ∪ dom(R)`
2. **Same iteration order:** When flattened, `iterate(Overlay(L, R))` produces keys in the same order as `L ⊕ R`
3. **Same sharing:** Overlay access must preserve the `Rc::clone` contract from P8 — `Overlay(L, R)(k)` returns `Rc::clone` of `R(k)` or `L(k)`, the same `Rc<Thunk>` that eager merge would produce. This is pointer-level identity (`Rc::ptr_eq`), not just logical equivalence.

The overlay introduces two observable differences, both intentional:

**Error timing:** Eager merge materializes both dicts at merge time; overlay defers materialization of *both* L and R until access or iteration. A dict that would fail materialization (e.g., contains a cycle) fails at merge time with eager semantics but at access time with overlay semantics. This is documented as an intentional change in §Current vs Planned Laziness Analysis.

**Error ordering:** When both operands contain errors, eager merge reports L's error first (L is materialized before R at `builtins.rs:446-447`). Overlay reports whichever operand's error is triggered first by access patterns. Programs should not depend on which operand's error is reported when both are broken.

**Chained overlays:** `Overlay(Overlay(A, B), C)` has O(k) access per key for k chained merges. Flattening on iteration prevents unbounded chain depth during traversal. Overlay chain traversal is structural (key lookup, not thunk forcing) and does not consume depth budget from `MAX_EVAL_DEPTH` — it is analogous to `$get` on a nested scope chain, not to recursive materialization.

##### Part 6: Implementation Correspondence

| Spec element | Implementation |
|-------------|----------------|
| MERGE rule | `builtin_merge` (`builtins.rs:435-462`) |
| `materialize(θ_L, _, d)` | `materialize(&args[0], None, depth)` (line 446) |
| `materialize(θ_R, _, d)` | `materialize(&args[1], None, depth)` (line 447) |
| `require_dict` | `require_dict("merge", left_val, call_span)` (lines 448-449) |
| LEFT-KEEP | First loop: `result.insert(key.clone(), Rc::clone(thunk))` (lines 455-457) |
| RIGHT-BIAS | Second loop: `result.insert(key.clone(), Rc::clone(thunk))` (lines 459-461) |
| Iteration order | IndexMap preserves insertion order; `insert` on existing key replaces value at existing position |
| Value preservation (P8) | `Rc::clone(thunk)` — pointer copy, no materialization |
| `reject_named` | `reject_named("merge", named, call_span)` (line 442) |
| Arity check | `args.len() != 2` (line 443) |

##### Part 7: Worked Example

```lisp
base:  [timeout: 30  retries: 3  env: staging]
prod:  [call $merge $base [env: prod  timeout: 60]]
```

Applying MERGE:

```
L = {timeout↦θ(30), retries↦θ(3), env↦θ("staging")}
R = {env↦θ("prod"), timeout↦θ(60)}

K(L) = {timeout, retries, env}
K(R) = {env, timeout}
K(L) ∩ K(R) = {timeout, env}    (shared keys — R wins)
K(R) \ K(L) = ∅                  (no new keys from R)

L ⊕ R:
  timeout → θ(60)     [RIGHT-BIAS: R has timeout]     pos 0 (from L)
  retries → θ(3)      [LEFT-KEEP: only in L]          pos 1 (from L)
  env     → θ("prod") [RIGHT-BIAS: R has env]         pos 2 (from L)

Result: {timeout↦θ(60), retries↦θ(3), env↦θ("prod")}
```

Note: `timeout` stays at position 0 (its position in L), not position 1 (its position in R). `retries` stays at position 1. No new keys from R, so nothing appended. Values `θ(60)`, `θ(3)`, `θ("prod")` are thunk pointers — the integers and string are not materialized by `$merge`.

#### Error Semantics — Formal Specification

This section formalizes how errors are represented, propagated, decorated, memoized, and caught. It builds on the Failed state and FORCE-FAILED rule from §Thunk Lifecycle — Formal Specification and the error classes from §Call Convention — Part 4: Error Taxonomy. Error message formats and span assignments are specified in SPEC.md §9 Error Messages.

##### Part 1: Error Representation

An evaluation error `ε` is a record with four fields:

```
ε = ⟨kind, def_span, mat_span?, stack⟩  where
  kind      : ErrorKind       — structured error variant with domain-specific data
  def_span  : Span            — where the problematic value was defined
  mat_span  : Option<Span>    — where the value was first forced (if different)
  stack     : [StackFrame]    — chain of materialization contexts, outermost last
```

**Dual-span model:** Every error carries two source locations: the **definition site** (where the error-producing expression was written) and the **materialization site** (where a consumer forced the thunk that failed). When these coincide, `mat_span` is `None`. When a Failed thunk is re-accessed from a third location, the new access site is pushed onto `stack` as a frame — `def_span` and `mat_span` are never overwritten after initial assignment.

**Stack frames:** Each frame is `⟨label, span⟩` where `label` identifies the context (e.g., the thunk's origin name, `"materialized"` for re-access) and `span` is the source location. Frames are added by `attach_materialization_context` during propagation and by the Failed state handler during re-access.

##### Part 2: Error Sources

All errors are constructed via `EvalError` methods that create an error with a specific `ErrorKind` variant. The main named constructors are:

| Constructor | ErrorKind Variant | Message Pattern | `def_span` Source |
|------------|-------------------|----------------|-------------------|
| `key_not_found(key, span)` | `KeyNotFound { key }` | `"key not found: {key}"` | Access expression |
| `type_mismatch(expected, got, span)` | `TypeMismatch { context: None, expected, got }` | `"type mismatch: expected {expected}, got {got}"` | Expression producing wrong type |
| `type_mismatch_ctx(context, expected, got, span)` | `TypeMismatch { context: Some(context), expected, got }` | `"{context}: expected {expected}, got {got}"` | Expression producing wrong type |
| `arity_mismatch(expected, got, span)` | `ArityMismatch { expected, got }` | `"arity mismatch: expected {expected} arguments, got {got}"` | Call expression |
| `circular_dependency(name, span)` | `CircularDependency { name }` | `"circular dependency detected while evaluating {name}"` | Thunk definition |
| `depth_exceeded(limit, span)` | `DepthExceeded { limit }` | `"maximum evaluation depth exceeded ({limit})"` | Thunk being forced when limit hit |
| `user_error(message, span)` | `UserError { message }` | `"{message}"` (user-provided) | `$error` call site |
| `integer_overflow(op, span)` | `IntegerOverflow { op }` | `"{op}: integer overflow"` | Arithmetic expression |
| `division_by_zero(op, span)` | `DivisionByZero { op }` | `"{op}: division by zero"` | Division expression |
| `float_not_finite(builtin, value, span)` | `FloatNotFinite { builtin, value }` | `"{builtin}: {value} is not a finite number"` | Builtin call expression |
| `empty_collection(op, span)` | `EmptyCollection { op }` | `"{op} on empty collection"` | Builtin call expression |
| `named_arg_rejected(builtin, span)` | `NamedArgRejected { builtin }` | `"{builtin} does not accept named arguments"` | Call expression |
| `internal(message, span)` | `Internal { message }` | `"{message}"` (implementation-defined) | Context-dependent |

See `src/error.rs` for the full set of 26 `ErrorKind` variants and their constructors. Additional variants not listed above include: `UndefinedVariable`, `TypeAssertFailed`, `NamedArgConflict`, `UnknownNamedArg`, `DuplicateKey`, `JsonDepthExceeded`, `IncludeNotAvailable`, `IncludeIoError`, `IncludeCycle`, `IncludeParseFailed`, `IncludeFileTooLarge`, `ParseConversion`, `JsonParse`, and `JsonRange`.

**Special error properties:**

- **`DepthExceeded` is not catchable:** `$try` does not catch `DepthExceeded` errors — they propagate to the runtime. Resource limit errors like stack overflow should not be suppressible by user code (follows GHC's `StackOverflow` and Racket's `exn:fail:resource` semantics). The `is_catchable()` method returns `false` for `DepthExceeded`, `true` for all other variants.

- **`DepthExceeded` is not cacheable:** Failed thunk state does not cache `DepthExceeded` errors — a thunk that fails at one depth may succeed at a shallower depth. The `is_cacheable()` method returns `false` for `DepthExceeded`, `true` for all other variants. This implements the PROP-DEPTH non-memoization rule from Part 5.

##### Part 3: Error Decoration

**[DECORATE]** — `attach_materialization_context(ε, mat_span, origin, thunk_span)`:

```
DECORATE(ε, mat_span, origin, thunk_span):
  (1) if mat_span is Some(s) ∧ ε.mat_span is None:
        ε.mat_span ← Some(s)
  (2) if mat_span is Some(s) ∧ ε.mat_span is Some(s') ∧ s ≠ s'
        ∧ s ∉ {f.span | f ∈ ε.stack}:
        ε.stack.push(⟨"materialized", s⟩)
  (3) if origin ≠ "" ∧ ∄f ∈ ε.stack. f.label = origin ∧ f.span = thunk_span:
        ε.stack.push(⟨origin, thunk_span⟩)
```

Rule (1) sets the materialization span on first decoration. Rule (2) adds subsequent materialization sites as stack frames without overwriting the original. Rule (3) adds the thunk's origin label (e.g., variable name) as a frame — the `origin` parameter corresponds to the thunk's origin name as described in §Scope Chain Semantics — Formal Specification. The deduplication guards (`s ∉ stack`, `∄f matching (label, span)`) prevent redundant frames when the same span propagates through nested `materialize` calls.

**Invariant:** `ε.mat_span`, once set to `Some(s)`, is never changed to `Some(s')` where `s ≠ s'`. The materialization span records the *first* site that forced the thunk; subsequent sites become stack frames.

##### Part 4: Error Propagation

Errors propagate upward through materialization chains via Rust's `?` operator (early return of `Result::Err`). Every `materialize` call site is a potential decoration point.

**[PROP-EVAL]** — Unevaluated thunk evaluation:

```
eval(expr, env, d+1) ⇒ Err(ε)
ε' = DECORATE(ε, mat_span, origin, thunk_span)
if ε'.kind.is_cacheable():
  thunk.state ← Failed(ε')
else:
  thunk.state ← Unevaluated(expr, env)   // restore original state
──────────────────────────
materialize(thunk, mat_span, d) ⇒ Err(ε')
```

Note: `eval()` may internally call `materialize()` recursively (e.g., for PendingCall resolution). PROP-EVAL covers the outer eval → error path; nested materialization within eval follows PROP-RESULT or PROP-BUILTIN depending on the thunk state encountered.

**[PROP-BUILTIN]** — PendingBuiltin execution:

```
func(args, named, pd, cs) ⇒ Err(ε)
ε' = DECORATE(ε, mat_span, origin, thunk_span)
if ε'.kind.is_cacheable():
  thunk.state ← Failed(ε')
else:
  thunk.state ← PendingBuiltin(func, args, named, pd, cs)   // restore
──────────────────────────
materialize(thunk, mat_span, d) ⇒ Err(ε')
```

**[PROP-RESULT]** — Recursive materialization of result thunk:

```
func(...) ⇒ Ok(θ_result)
materialize(θ_result, mat_span, d+1) ⇒ Err(ε)
ε' = DECORATE(ε, mat_span, origin, thunk_span)
if ε'.kind.is_cacheable():
  thunk.state ← Failed(ε')
else:
  thunk.state ← restore(original_state)   // restore pre-InProgress state
──────────────────────────
materialize(thunk, mat_span, d) ⇒ Err(ε')
```

**State restoration for non-cacheable errors:** `is_cacheable()` returns `false` only for `DepthExceeded`. When a non-cacheable error occurs, the thunk's original state (Unevaluated, PendingBuiltin, or PendingCall) is restored instead of transitioning to Failed. This preserves the PROP-DEPTH invariant: a thunk that fails at depth N may succeed at depth N-1, so its semantic state must remain "not yet computed." The restoration is a backward transition in the state DAG (InProgress → original state), which is sound because `DepthExceeded` is an administrative interruption, not a semantic failure (see §Thunk Lifecycle, Semantic Commitment #3).

**PendingCall coverage:** PendingCall thunks have four error paths (function materialization, invoke_function, result materialization, type mismatch). All follow the same DECORATE + conditional-cache pattern: function materialization failures and type mismatches are decorated inline; result materialization follows PROP-RESULT; invoke_function failures are decorated and conditionally cached. PendingCall restoration requires cloning `func`, `args`, and `named` before evaluation (all `Rc::clone` — no materialization) since `take_pending_call()` consumes ownership.

**[PROP-CYCLE]** — Circular dependency:

```
thunk.state = InProgress
ε = circular_dependency(name, thunk.span)
ε.mat_span ← mat_span
thunk.state ← Failed(ε)
──────────────────────────
materialize(thunk, mat_span, d) ⇒ Err(ε)
```

Note: PROP-CYCLE constructs the error inline at the detection site — it does *not* pass through DECORATE. This is the only propagation rule that bypasses decoration, because the error originates at the forcing site itself rather than propagating from a deeper call.

**[PROP-DEPTH]** — Depth limit exceeded:

```
d > MAX_EVAL_DEPTH
ε = depth_exceeded(MAX_EVAL_DEPTH, thunk.span)
ε.mat_span ← mat_span
──────────────────────────
materialize(thunk, mat_span, d) ⇒ Err(ε)
```

Note: PROP-DEPTH does *not* transition to Failed — the thunk state is unchanged (§Thunk Lifecycle, Semantic Commitment #3). The same thunk may succeed at a lower depth.

**Propagation path:** In a chain `θ₁ → θ₂ → θ₃` where θ₃ fails, the error propagates θ₃ → θ₂ → θ₁, each level applying DECORATE. The result is an error with `def_span` from θ₃ (where the problem was defined), `mat_span` from the first forcing site, and stack frames from intermediate materialization points.

##### Part 5: Failed State Memoization

**[MEMO-CACHE]** — On first error, cache in Failed state:

```
materialize(thunk, ...) ⇒ Err(ε)
ε.kind.is_cacheable()
──────────────────────────
thunk.state ← Failed(ε)
```

**[MEMO-SKIP]** — Non-cacheable error, restore thunk state:

```
materialize(thunk, ...) ⇒ Err(ε)
¬ε.kind.is_cacheable()
──────────────────────────
thunk.state ← restore(original_state)   // pre-InProgress state
```

Cacheable error paths (PROP-EVAL, PROP-BUILTIN, PROP-RESULT, PROP-CYCLE) cache via `cache_failure`. Non-cacheable errors (DepthExceeded) restore the thunk to its pre-InProgress state via MEMO-SKIP, allowing the same thunk to succeed at a shallower call depth. The cached error includes decoration from DECORATE — `mat_span` and stack frames from the first materialization chain are preserved.

**[MEMO-REACCESS]** — On subsequent access of a Failed thunk:

```
thunk.state = Failed(ε_cached)
ε' = clone(ε_cached)
(1) if mat_span is Some(s) ∧ ε'.mat_span is None:
      ε'.mat_span ← Some(s)
      update cache: thunk.state ← Failed(ε')
(2) if mat_span is Some(s) ∧ ε'.mat_span is Some(s') ∧ s ≠ s'
      ∧ s ∉ {f.span | f ∈ ε'.stack}:
      ε'.stack.push(⟨"materialized", s⟩)
      update cache: thunk.state ← Failed(ε')
(3) if mat_span is None:
      (no decoration, no cache update — return ε' unchanged)
──────────────────────────
materialize(thunk, mat_span, d) ⇒ Err(ε')
```

MEMO-REACCESS mirrors DECORATE but operates on the cached error. Cache updates are progressive: each new access site enriches the cached error's stack. This is the Failed self-edge in the thunk lifecycle DAG — it refines diagnostic metadata without changing the error's semantic content (message, def_span).

**Permanence:** Once a thunk reaches Failed, it never returns to any other state (§Thunk Lifecycle, Semantic Commitment #1). No retry, no recovery. This includes I/O failures from `$include`. The only exception: non-cacheable errors (DepthExceeded) trigger MEMO-SKIP instead of MEMO-CACHE — the thunk state is restored rather than transitioning to Failed, because depth errors are context-dependent, not intrinsic to the thunk.

##### Part 6: `$try` Catching Boundary

**[TRY]** — Error catching:

```
materialize(θ_func, _, d) ⇒ Function([], body, env)
θ_body = Thunk::new_unevaluated(body, env, body.span)
materialize(θ_body, _, d) ⇒ Ok(v)
──────────────────────────
try(θ_func, d, s) ⇒ ok_val(Dict({ok ↦ θ(v)}))
```

**[TRY-ERR]** — Error caught:

```
materialize(θ_func, _, d) ⇒ Function([], body, env)
θ_body = Thunk::new_unevaluated(body, env, body.span)
materialize(θ_body, _, d) ⇒ Err(ε)
ε.kind.is_catchable()
──────────────────────────
try(θ_func, d, s) ⇒ ok_val(Dict({err ↦ θ(ε.kind.to_string())}))
```

**[TRY-UNCATCHABLE]** — Uncatchable error re-raised:

```
materialize(θ_func, _, d) ⇒ Function([], body, env)
θ_body = Thunk::new_unevaluated(body, env, body.span)
materialize(θ_body, _, d) ⇒ Err(ε)
¬ε.kind.is_catchable()
──────────────────────────
try(θ_func, d, s) ⇒ Err(ε)
```

**[TRY-BUILTIN]** — Builtin zero-arg function:

```
materialize(θ_func, _, d) ⇒ Builtin(f)
f([], {}, d, s) ⇒ Ok(θ_result)
materialize(θ_result, _, d) ⇒ Ok(v)
──────────────────────────
try(θ_func, d, s) ⇒ ok_val(Dict({ok ↦ θ(v)}))
```

(Catchable error variant: same structure, `Err(ε), ε.kind.is_catchable() ⇒ Dict({err ↦ θ(ε.kind.to_string())})`; uncatchable errors re-raised per TRY-UNCATCHABLE)

**Catching boundary:** `$try` catches errors at the zero-argument function body boundary. The function is materialized *outside* the catch — if the function thunk itself fails to materialize, that error propagates to `$try`'s caller (not caught). Only errors from *calling* the function (evaluating its body) are caught.

**Error-to-value conversion:** `$try` extracts only the message string (`ε.kind.to_string()`). The spans and stack frames are discarded — `$try` is for program-level error handling, not diagnostic reporting. The result is an ordinary dict with key `ok` or `err`, not a special type.

**Arity constraint:** The function must take zero parameters. If `params.len() > 0`, `$try` raises an error (not caught): `"try: expected a zero-argument function, got {n} parameters"`.

**Interaction with Failed state:** When `$try` forces a Failed thunk *inside* the body, the cached error is returned via MEMO-REACCESS and caught by `$try`. The Failed thunk's cache is updated (stack frame added) but the error is converted to `[err: message]` — it does not propagate past `$try`.

##### Part 7: Properties

**E1 — Error determinism:** For a given program state (environment, thunk graph), the same error is produced regardless of evaluation order. This follows from the pure subset's confluence (§Thunk Lifecycle, Semantic Properties). `$include` breaks this — file system state introduces nondeterminism.

**E2 — Memoization permanence:** `Failed(ε)` is absorbing — no transition out of Failed exists. Formally: if `thunk.state = Failed(ε)` at time t, then `thunk.state = Failed(ε')` for all t' > t, where `ε'.kind = ε.kind ∧ ε'.def_span = ε.def_span ∧ (ε.mat_span = Some(s) → ε'.mat_span = Some(s))` — mat_span may transition from None to Some but never from Some(s) to Some(s') where s ≠ s'. Stack frames may grow monotonically.

**E3 — Propagation preserves definition site:** DECORATE never modifies `ε.def_span`. The definition site is set at error construction and propagated unchanged through any number of materialization layers.

**E4 — Materialization site is first-access:** `ε.mat_span` records the first site that triggered materialization. Subsequent access sites become stack frames. This is enforced by DECORATE rule (1) (set only if None) and MEMO-REACCESS rule (1).

**E5 — `$try` isolation:** Errors caught by `$try` do not propagate to `$try`'s caller. `$try` converts errors to values — the error is consumed, not rethrown. There is no `$rethrow` mechanism.

**E6 — Depth errors are non-caching:** DepthExceeded errors have `is_cacheable() = false`, triggering MEMO-SKIP instead of MEMO-CACHE. The thunk state is restored to its pre-InProgress state, allowing the same thunk to succeed at a shallower call depth. This is the only error source that does not cache.

**E7 — Stack frame monotonicity:** The `stack` field of a cached error grows monotonically — frames are appended, never removed or reordered. Each re-access of a Failed thunk from a new location adds at most one frame.

**E8 — DECORATE idempotence:** Applying DECORATE twice with the same arguments produces the same result as applying it once: `DECORATE(DECORATE(ε, s, o, t), s, o, t) = DECORATE(ε, s, o, t)`. This follows from the deduplication guards in rules (1)–(3).

**Typing:** `$try` has type `Any → Any` — more precisely it expects `Fn(→ τ)` and returns `[ok: τ] | [err: Str]`, but neither the constraint on the argument nor the union result type can be expressed without union types (Phase 3+). `$error` has type `Str → Any` — the argument is materialized and coerced to String; the return type is `Any` because the function never returns a value (it always raises an error), and tinct has no bottom type.

**Runtime vs. static errors:** Runtime errors (`EvalError`, cached in `Failed` thunks) are distinct from the type inference engine's `Type::Error` marker. `Type::Error` represents the type of expressions that are statically known to produce errors (e.g., undefined variables caught during type checking); `EvalError` is the runtime value produced during evaluation.

##### Part 8: Implementation Correspondence

| Spec element | Implementation |
|-------------|----------------|
| EvalError struct | `error.rs:408-413` |
| DECORATE | `attach_materialization_context` (`eval.rs:815-843`) |
| PROP-EVAL | `eval.rs:931-951` (Unevaluated path, `map_err(&decorate)` + conditional cache) |
| PROP-BUILTIN | `eval.rs:952-1003` (PendingBuiltin path) |
| PROP-RESULT | `eval.rs:967-987`, `eval.rs:1044-1063` (recursive materialize of result) |
| PROP-CYCLE | `eval.rs:908-922` (InProgress handler, inline error construction) |
| PROP-DEPTH | `eval.rs:869-875` (depth check, no state change) |
| MEMO-CACHE | `thunk.cache_failure` (`value.rs:384-386`) |
| MEMO-SKIP | `eval.rs:944-948`, `eval.rs:976-983`, `eval.rs:993-999` (non-cacheable state restore) |
| MEMO-REACCESS | `eval.rs:885-906` (Failed state handler) |
| TRY | `builtin_try` (`builtins.rs:800-884`) |
| TRY-UNCATCHABLE | `builtins.rs:870-871` (`!e.kind.is_catchable()` re-raise) |
| TRY catching boundary | `builtins.rs:837` (body materialize inside match) |
| Error-to-value | `builtins.rs:873-881` (extract `e.message()`, delegates to `e.kind.to_string()`) |
| $error | `builtin_error` (`builtins.rs:785-795`) |

#### Structured Error Model

This section specifies the structured representation that replaces the freeform `message: String` field in `EvalError`. The error semantics (propagation, decoration, memoization, catching) remain unchanged — this section restructures error **identity and data** only.

##### Motivation

The current `EvalError` carries `message: String` as its primary payload. Of ~130 error construction sites, 85 use the freeform `EvalError::new(message, span)` constructor. This prevents:

1. **Programmatic error identity** — tests match substrings; tooling cannot branch on error kind.
2. **Structured data extraction** — `"key not found: foo"` buries the key name in the message; "did you mean?" suggestions require parsing it back out.
3. **Error codes** — no stable identifiers for `tinct explain` or documentation linking.
4. **Multi-format rendering** — message text is baked into construction sites, blocking JSON output, LSP diagnostics, and format-independent rendering.

##### Design: `ErrorKind` Enum

Replace the `message: String` field in `EvalError` with `kind: ErrorKind`. Each variant carries structured domain data. Human-readable messages are derived via `Display` on `ErrorKind`, not stored.

```rust
pub struct EvalError {
    pub kind: ErrorKind,
    pub definition_span: Span,
    pub materialization_span: Option<Span>,
    pub stack: Vec<StackFrame>,
}
```

##### Part 1: Variant Catalog

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ErrorKind {
    // --- Access errors (E000-E009) ---
    KeyNotFound { key: String },
    /// `name` stores the identifier without `$` prefix (e.g., `"x"` not `"$x"`).
    /// Display adds the `$` back: `"undefined variable: $x"`.
    UndefinedVariable { name: String },

    // --- Type errors (E010-E019) ---
    /// Runtime type mismatch from evaluator or builtin dispatch.
    /// `context` carries the builtin name (e.g., `"merge"`) when the mismatch
    /// originates from a builtin; `None` for generic evaluator mismatches.
    /// `expected` is human-readable, not machine-parseable — may contain
    /// compound descriptions like `"Dict or Seq"`.
    TypeMismatch { context: Option<String>, expected: String, got: String },
    /// User-written type assertion (`[@Type value]`) failed at runtime.
    /// Semantically distinct from `TypeMismatch` — this is a user-authored
    /// type guard, not an internal evaluator check.
    TypeAssertFailed { expected: String, got: String },

    // --- Call errors (E020-E029) ---
    ArityMismatch { expected: ArityBound, got: usize },
    NamedArgConflict { param: String },
    UnknownNamedArg { name: String },
    NamedArgRejected { builtin: String },

    // --- Value errors (E030-E039) ---
    DuplicateKey { key: String },
    /// `op` carries the operator symbol (e.g., `"/"`) for Display prefix.
    DivisionByZero { op: String },
    IntegerOverflow { op: String },
    /// Covers NaN, Infinity, and -Infinity — values that are not finite
    /// and cannot be converted to Int or used in contexts requiring finite floats.
    FloatNotFinite { builtin: String, value: f64 },
    EmptyCollection { op: String },

    // --- Limit errors (E040-E049) ---
    /// Evaluation depth limit (recursive thunk forcing).
    DepthExceeded { limit: usize },
    /// JSON nesting depth limit (distinct from eval depth — applies during
    /// `$from-json` parsing of deeply nested JSON structures).
    JsonDepthExceeded { limit: usize },

    // --- Include errors (E050-E059) ---
    IncludeNotAvailable,
    /// Covers both "cannot open" (canonicalize failure) and "cannot read"
    /// (metadata/read failure). The `detail` field carries the OS error.
    IncludeIoError { path: String, detail: String },
    IncludeCycle { path: String },
    IncludeParseFailed { path: String, detail: String },
    IncludeFileTooLarge { path: String, size: u64, limit: u64 },

    // --- Conversion errors (E060-E069) ---
    ParseConversion { builtin: String, input: String, target: String },
    JsonParse { detail: String },
    JsonRange,

    // --- Evaluation structure (E070-E079) ---
    CircularDependency { name: String },

    // --- User-generated (E080-E089) ---
    UserError { message: String },

    // --- Escape hatch (E090-E099) ---
    Internal { message: String },
}
```

The `ArityBound` type expresses flexible arity constraints:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ArityBound {
    Exact(usize),
    AtMost(usize),
    Range(usize, usize),
}
```

**Variant design principles:**

- **One variant per user-distinguishable error class.** If two errors should produce different suggestions, they get different variants. If they differ only in wording, they share a variant with a field.
- **`TypeMismatch` vs `TypeAssertFailed`:** `TypeMismatch` is an evaluator/builtin dispatch error ("merge got the wrong type"). `TypeAssertFailed` is a user-written runtime type guard failure (`[@Int "hello"]`). Different error class, different suggestions, different error code. When `ThunkState::Guarded` validation (§TypeAssert Runtime Validation) is implemented, guard failures produce `TypeAssertFailed`.
- **`context: Option<String>` in `TypeMismatch`** carries the builtin name when the mismatch originates from a builtin (e.g., `"merge"` in `"merge: expected Dict, got Int"`). `None` for generic type mismatches from the evaluator. The `expected` field is human-readable, not machine-parseable — it may contain compound descriptions like `"Dict or Seq"`. Programmatic matching on expected types is not supported; use the error code and `context` field instead.
- **`DivisionByZero` carries `op`** to preserve the operator prefix in Display output (e.g., `"/: division by zero"`). This maintains `$try` message compatibility and future-proofs for additional division operators.
- **`FloatNotFinite`** covers NaN, Infinity, and -Infinity — all non-finite `f64` values. Named `NotFinite` rather than `OutOfRange` because NaN is not a range concept.
- **`DepthExceeded` vs `JsonDepthExceeded`:** Eval depth (recursive thunk forcing) and JSON nesting depth (`$from-json` parsing) are semantically different limits with different error codes. A JSON depth error at E041 does not indicate runaway evaluation.
- **`IncludeIoError`** covers both "cannot open" (canonicalize failure) and "cannot read" (metadata/read failure) — both are filesystem IO failures distinguished by the `detail` field.
- **`Internal` is an escape hatch**, not a permanent category. It accepts a freeform message string for incremental migration. New error sites should use a typed variant; `Internal` should trend toward zero usage over time.
- **Terminology:** "Type error" in this section always means a *runtime* type mismatch (`ErrorKind::TypeMismatch`). Static type checking failures are `TypeError` in `src/types.rs` — a separate type, separate system, separate error reporting path.

##### Part 2: Error Codes

Each variant maps to a stable error code. Codes are `E` followed by a three-digit number, grouped by domain.

**Stability principle:** Error codes are part of tinct's public interface. A code, once assigned, always means the same error class across all releases. Codes are never reassigned to different error classes. This enables `tinct explain E001`, programmatic error filtering, and documentation linking.

| Code | Variant | Category |
|------|---------|----------|
| E001 | `KeyNotFound` | Access |
| E002 | `UndefinedVariable` | Access |
| E010 | `TypeMismatch` | Type |
| E011 | `TypeAssertFailed` | Type |
| E020 | `ArityMismatch` | Call |
| E021 | `NamedArgConflict` | Call |
| E022 | `UnknownNamedArg` | Call |
| E023 | `NamedArgRejected` | Call |
| E030 | `DuplicateKey` | Value |
| E031 | `DivisionByZero` | Value |
| E032 | `IntegerOverflow` | Value |
| E033 | `FloatNotFinite` | Value |
| E034 | `EmptyCollection` | Value |
| E040 | `DepthExceeded` | Limit |
| E041 | `JsonDepthExceeded` | Limit |
| E050 | `IncludeNotAvailable` | Include |
| E051 | `IncludeIoError` | Include |
| E052 | `IncludeCycle` | Include |
| E053 | `IncludeParseFailed` | Include |
| E054 | `IncludeFileTooLarge` | Include |
| E060 | `ParseConversion` | Conversion |
| E061 | `JsonParse` | Conversion |
| E062 | `JsonRange` | Conversion |
| E070 | `CircularDependency` | Evaluation |
| E080 | `UserError` | User |
| E099 | `Internal` | Internal |

**Numbering scheme:** Codes are grouped in decades by domain with explicit ranges:

| Range | Domain |
|-------|--------|
| E000–E009 | Access |
| E010–E019 | Type |
| E020–E029 | Call |
| E030–E039 | Value |
| E040–E049 | Limit |
| E050–E059 | Include |
| E060–E069 | Conversion |
| E070–E079 | Evaluation |
| E080–E089 | User |
| E090–E099 | Internal |

Gaps between codes within each range allow inserting new variants without renumbering existing codes.

Error codes are derived from the variant via a method:

```rust
impl ErrorKind {
    pub fn code(&self) -> &'static str {
        match self {
            Self::KeyNotFound { .. } => "E001",
            Self::UndefinedVariable { .. } => "E002",
            Self::TypeMismatch { .. } => "E010",
            Self::TypeAssertFailed { .. } => "E011",
            Self::ArityMismatch { .. } => "E020",
            Self::NamedArgConflict { .. } => "E021",
            Self::UnknownNamedArg { .. } => "E022",
            Self::NamedArgRejected { .. } => "E023",
            Self::DuplicateKey { .. } => "E030",
            Self::DivisionByZero { .. } => "E031",
            Self::IntegerOverflow { .. } => "E032",
            Self::FloatNotFinite { .. } => "E033",
            Self::EmptyCollection { .. } => "E034",
            Self::DepthExceeded { .. } => "E040",
            Self::JsonDepthExceeded { .. } => "E041",
            Self::IncludeNotAvailable => "E050",
            Self::IncludeIoError { .. } => "E051",
            Self::IncludeCycle { .. } => "E052",
            Self::IncludeParseFailed { .. } => "E053",
            Self::IncludeFileTooLarge { .. } => "E054",
            Self::ParseConversion { .. } => "E060",
            Self::JsonParse { .. } => "E061",
            Self::JsonRange => "E062",
            Self::CircularDependency { .. } => "E070",
            Self::UserError { .. } => "E080",
            Self::Internal { .. } => "E099",
        }
    }

    /// Returns `false` for errors that must not be cached in Failed thunk state.
    /// Currently only `DepthExceeded` — a thunk that fails at one depth may
    /// succeed at a shallower depth (PROP-DEPTH in §Error Semantics).
    pub fn is_cacheable(&self) -> bool {
        !matches!(self, Self::DepthExceeded { .. })
    }
}
```

##### Part 3: Message Generation

`Display` on `ErrorKind` generates human-readable messages. Messages follow rustc style guidelines:

1. **No trailing punctuation.** `"key not found: foo"` not `"key not found: foo."`
2. **Lowercase start.** `"expected Dict, got Int"` not `"Expected Dict, got Int"`
3. **No questions.** `"type mismatch: expected Int, got String"` not `"did you expect Int?"`
4. **May contain names.** `"undefined variable: $x"` — include the identifier
5. **No internal jargon.** Never reference "thunk", "materialization", "PendingCall", or "Unevaluated" in user-facing messages

```rust
impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyNotFound { key } =>
                write!(f, "key not found: {key}"),
            Self::UndefinedVariable { name } =>
                write!(f, "undefined variable: ${name}"),
            Self::TypeMismatch { context: Some(ctx), expected, got } =>
                write!(f, "{ctx}: expected {expected}, got {got}"),
            Self::TypeMismatch { context: None, expected, got } =>
                write!(f, "type mismatch: expected {expected}, got {got}"),
            Self::TypeAssertFailed { expected, got } =>
                write!(f, "type assertion failed: expected {expected}, got {got}"),
            Self::ArityMismatch { expected, got } =>
                write!(f, "arity mismatch: expected {expected}, got {got}"),
            Self::NamedArgConflict { param } =>
                write!(f, "parameter '{param}' received both positional and named argument"),
            Self::UnknownNamedArg { name } =>
                write!(f, "unexpected named argument: {name}"),
            Self::NamedArgRejected { builtin } =>
                write!(f, "{builtin} does not accept named arguments"),
            Self::DuplicateKey { key } =>
                write!(f, "duplicate key: {key}"),
            Self::DivisionByZero { op } =>
                write!(f, "{op}: division by zero"),
            Self::IntegerOverflow { op } =>
                write!(f, "{op}: integer overflow"),
            Self::FloatNotFinite { builtin, value } =>
                write!(f, "{builtin}: {value} is not a finite number"),
            Self::EmptyCollection { op } =>
                write!(f, "{op} on empty collection"),
            Self::DepthExceeded { limit } =>
                write!(f, "maximum evaluation depth exceeded ({limit})"),
            Self::JsonDepthExceeded { limit } =>
                write!(f, "maximum JSON nesting depth exceeded ({limit})"),
            Self::IncludeNotAvailable =>
                write!(f, "include: not available in this context"),
            Self::IncludeIoError { path, detail } =>
                write!(f, "include: cannot access \"{path}\": {detail}"),
            Self::IncludeCycle { path } =>
                write!(f, "circular include detected: \"{path}\""),
            Self::IncludeParseFailed { path, detail } =>
                write!(f, "include: parse error in \"{path}\": {detail}"),
            Self::IncludeFileTooLarge { path, size, limit } =>
                write!(f, "include: file \"{path}\" is {size} bytes, exceeds {limit} byte limit"),
            Self::ParseConversion { builtin, input, target } =>
                write!(f, "{builtin}: cannot parse {input:?} as {target}"),
            Self::JsonParse { detail } =>
                write!(f, "from-json: invalid JSON: {detail}"),
            Self::JsonRange =>
                write!(f, "JSON number outside representable range"),
            Self::CircularDependency { name } =>
                write!(f, "circular dependency detected while evaluating {name}"),
            Self::UserError { message } =>
                write!(f, "{message}"),
            Self::Internal { message } =>
                write!(f, "{message}"),
        }
    }
}
```

`ArityBound` displays as natural language:

```rust
impl fmt::Display for ArityBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(n) => write!(f, "{n} arguments"),
            Self::AtMost(n) => write!(f, "at most {n} arguments"),
            Self::Range(lo, hi) => write!(f, "{lo} to {hi} arguments"),
        }
    }
}
```

##### Part 4: EvalError Display

`EvalError::Display` changes to include the error code:

```rust
impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {} (defined at {})", self.kind.code(), self.kind, self.definition_span)?;
        if let Some(ref mat_span) = self.materialization_span {
            write!(f, " (materialized at {mat_span})")?;
        }
        for frame in &self.stack {
            write!(f, "\n  in {} at {}", frame.label, frame.span)?;
        }
        Ok(())
    }
}
```

Example output: `[E001] key not found: name (defined at 3:5-3:10) (materialized at 7:1-7:8)`

##### Part 5: Constructor Migration

Named constructors become typed:

```rust
impl EvalError {
    pub fn key_not_found(key: &str, span: Span) -> Self {
        Self { kind: ErrorKind::KeyNotFound { key: key.to_string() }, definition_span: span, materialization_span: None, stack: Vec::new() }
    }
    pub fn type_mismatch(expected: &str, got: &str, span: Span) -> Self {
        Self { kind: ErrorKind::TypeMismatch { context: None, expected: expected.to_string(), got: got.to_string() }, definition_span: span, materialization_span: None, stack: Vec::new() }
    }
    // ... etc for each variant with a convenience constructor
}
```

The freeform `EvalError::new(message, span)` is replaced by `EvalError::internal(message, span)` which constructs `ErrorKind::Internal`. This preserves backward compatibility during migration while making escape-hatch usage explicit and greppable.

##### Part 6: `$try` Interaction

`$try` currently extracts `ε.message` (a String) for the `[err: message]` result. With structured errors, `$try` extracts `ε.kind.to_string()` — the Display output of ErrorKind. This preserves the current behavior: the caught value is a human-readable error message string, not a structured error object. Error codes are not exposed through `$try`.

**Rationale:** `$try` is for program-level error recovery ("did it fail?"), not error introspection. Programs that need to distinguish error kinds should use type checking and validation, not `$try`-and-parse. Exposing structured error data through `$try` would create a coupling between error representation (an implementation detail) and user programs.

**Display stability:** Error codes are stable across releases (see Part 2). Display message *wording* is not part of the stability contract — message text may be refined for clarity across releases. Programs that match on `$try` error strings (e.g., `[call $= $result.err "division by zero"]`) are inherently fragile and should not rely on exact wording. This is consistent with SPEC.md §9.3: "Exact message wording may vary across releases."

##### Part 7: Rendering Separation

The `ErrorKind` enum separates error **data** from error **presentation**. The `Display` impl is the default text renderer. Future renderers can pattern-match on `ErrorKind` directly:

- **JSON output** (`--error-format json`): serialize `ErrorKind` variant name, structured fields, code, spans
- **LSP diagnostics**: map `ErrorKind` to `DiagnosticSeverity`, use structured fields for `relatedInformation`
- **Rich terminal** (`codespan-reporting`): use spans for source snippets with carets
- **`tinct explain E001`**: look up extended help text by error code

These renderers are not specified here — they are future work. The design enables them without changing error construction sites.

##### Part 8: Style Guidelines

Error messages (generated by `ErrorKind::Display`) follow these rules, adapted from rustc's diagnostic guidelines:

| Rule | Example | Counterexample |
|------|---------|----------------|
| Lowercase start | `"expected Dict, got Int"` | `"Expected Dict, got Int"` |
| No trailing punctuation | `"key not found: x"` | `"key not found: x."` |
| No questions | `"/: division by zero"` | `"did you divide by zero?"` |
| Include the value/name | `"undefined variable: $x"` | `"undefined variable"` |
| No internal jargon | `"circular dependency"` | `"thunk in InProgress state"` |
| Builtin prefix when relevant | `"merge: expected Dict"` | `"type mismatch in merge"` |
| Use "expected X, got Y" | `"expected Int, got String"` | `"Int required but String given"` |
| Error codes are stable | E001 always means `KeyNotFound` | Reassigning E001 to a different error class |

##### Part 9: Implementation Correspondence

| Spec element | Implementation |
|-------------|----------------|
| `ErrorKind` enum | `error.rs` (new) |
| `ArityBound` enum | `error.rs` (new) |
| Error codes | `ErrorKind::code()` method (`error.rs`) |
| Cacheability | `ErrorKind::is_cacheable()` method (`error.rs`) (defined, integration deferred to error-structured-migrate) |
| Message generation | `ErrorKind::Display` impl (`error.rs`) |
| `EvalError` struct | `error.rs:20-25` (modified: `message` → `kind`) |
| Constructor migration | `error.rs` (existing constructors updated) |
| `$try` extraction | `builtins.rs` builtin_try (`e.kind.to_string()`) |
| Freeform escape hatch | `EvalError::internal()` → `ErrorKind::Internal` |
| PROP-DEPTH non-caching | `eval.rs` depth check constructs `ErrorKind::DepthExceeded` (is_cacheable() integration deferred) |

#### Document Pipeline and $include — Formal Specification

This section formalizes the inter-file include mechanism. The intra-file document pipeline (`$$` threading via `---` boundaries) and intra-document scope chains are already formalized in §Scope Chain Semantics — Formal Specification (DOC-PIPELINE and SEQ-SCOPE rules, respectively). This section covers `$include`: path resolution, cycle detection, result caching, and the eager materialization invariant.

##### Part 1: Include State

The include system maintains mutable state `Σ` shared across nested include calls:

```
Σ = ⟨base_dir, guard, cache, stdlib_env⟩  where
  base_dir   : Path              — directory of the currently-evaluating file
  guard      : Set<Path>         — canonical paths currently being evaluated (cycle detection)
  cache      : Map<Path, Rc<Thunk>>  — canonical path → evaluated result (memoization)
  stdlib_env : ρ                 — environment for included files (builtins + stdlib)
```

`Σ` is stored in a thread-local (`INCLUDE_CTX`). All mutations are scoped: `guard` entries are pushed before recursion and popped after (even on error); `base_dir` is saved and restored around each include. `cache` entries are append-only — once a file is cached, its result is never replaced.

**Thread-local model:** `Σ` uses thread-local storage rather than parameter threading. This is an implementation choice (builtins receive `BuiltinArgs`, not `Σ`), not a semantic requirement. The planned EvalContext migration (§EvalContext) will replace the thread-local with an explicit `Rc<RefCell<EvalContext>>` parameter, but the formal semantics are unchanged — `Σ` transitions are the same regardless of how `Σ` is threaded.

##### Part 2: Path Resolution

**[RESOLVE]** — Path resolution and canonicalization:

```
resolve(path_str, Σ.base_dir):
  raw = Path::new(path_str)
  resolved = if raw.is_absolute() then raw
             else Σ.base_dir / raw
  canonical = canonicalize(resolved)       (resolves symlinks, normalizes ..)
  ────────────────────────────────────────
  ⇒ canonical : Path
```

Canonicalization serves two purposes: (1) cycle detection requires path identity — `./lib/../lib/utils.llt` and `lib/utils.llt` must resolve to the same key; (2) caching requires the same identity guarantee. Canonicalization fails with an I/O error if the path does not exist on the filesystem.

**Allowlist check (planned):** When the filesystem allowlist (§Sandboxing & Security) is implemented, an INCLUDE-DENY rule will be inserted between RESOLVE and INCLUDE-HIT, rejecting paths outside allowed directories before consulting the cache. The check ordering specified in §Sandboxing is: canonicalize → allowlist → cache → cycle → read.

##### Part 3: Include Rules

Three rules cover the three possible outcomes of an include call. They are checked in priority order: cache → cycle → evaluate. (When the allowlist is implemented, a fourth outcome — INCLUDE-DENY — will precede all three.)

In all rules below, `d` is the evaluation depth and `s` is the call-site span (used for error reporting but not for rule selection).

**[INCLUDE-HIT]** — Cache hit (memoized result):

```
resolve(path_str, Σ.base_dir) ⇒ canonical
canonical ∈ dom(Σ.cache)
────────────────────────────────────────
include(path_str, Σ, d, s) ⇒ Ok(Rc::clone(Σ.cache[canonical]))
```

Cache hits return a clone of the cached thunk pointer. No file I/O, no evaluation. This is Jsonnet-style import memoization: multiple includes of the same file share a single evaluation result.

**[INCLUDE-CYCLE]** — Cycle detection:

```
resolve(path_str, Σ.base_dir) ⇒ canonical
canonical ∉ dom(Σ.cache)
canonical ∈ Σ.guard
────────────────────────────────────────
include(path_str, Σ, d, s) ⇒ Err("circular include detected: {canonical}")
```

A file currently being evaluated (present in the guard set) cannot be included again. This catches direct cycles (`A includes A`) and transitive cycles (`A includes B includes A`). The error is raised at the include call site — no evaluation of the cyclic file is attempted.

**[INCLUDE-EVAL]** — Fresh evaluation:

```
resolve(path_str, Σ.base_dir) ⇒ canonical
canonical ∉ dom(Σ.cache)
canonical ∉ Σ.guard
assert file_size(canonical) ≤ MAX_FILE_SIZE             (10 MB; prevents resource exhaustion)
source = read_file(canonical)                           (I/O: file read)
file = parse(source)                                    (parse tinct source)

Σ.guard ← Σ.guard ∪ {canonical}                        (push guard)
saved_base = Σ.base_dir
Σ.base_dir ← parent(canonical)                         (set base_dir for nested includes)

θ = eval_file(file, Σ.stdlib_env, d + 1)               (evaluate all documents)
v = materialize(θ, None, d + 1)                         (EAGER materialization — see Part 4)

Σ.base_dir ← saved_base                                (restore base_dir)
Σ.guard ← Σ.guard \ {canonical}                        (pop guard)

θ_result = Materialized(v)                              (pure allocation — no evaluation)
Σ.cache[canonical] ← θ_result                          (cache result)
────────────────────────────────────────
include(path_str, Σ, d, s) ⇒ Ok(θ_result)
```

On error at any step (file read, parse, eval, materialize), the `base_dir` and `guard` are restored before the error propagates — the INCLUDE-RESTORE invariant (Property 3 below).

The `d + 1` depth propagation means nested includes consume evaluation depth. Deep include chains eventually hit `MAX_EVAL_DEPTH`, providing an independent bound on include recursion beyond the guard set.

The included file evaluates with `Σ.stdlib_env` as its root scope and `$$` initialized to the empty dict (`eval_file` passes `None` as `initial_input` to `eval_file_with_input`, which defaults to `Materialized(Dict([]))`). It does *not* receive the including file's scope chain — include isolation is strict (Property 5).

##### Part 4: Eager Materialization Invariant

`$include` is one of three builtins that eagerly materialize their result (the others are `$eval` and `$try`). `$include` uses single-level `materialize` (forces the outer dict but leaves nested values as thunks), while `$eval` uses `deep_materialize` (recursively forces all nested thunks with cycle detection). `$try` materializes the function body result to determine success or failure. The eager materialization in INCLUDE-EVAL is required for correctness of the guard-based cycle detection:

**Why not lazy?** If `$include` returned `θ` (the unevaluated result thunk) instead of `Materialized(v)`:

1. **Cycle detection breaks.** The guard entry for `canonical` is popped immediately after `eval_file` returns. A lazy result defers actual evaluation of nested `$include` calls within the result — when those deferred thunks are later forced, `canonical` is no longer in the guard set, so a transitive cycle would go undetected.

2. **Path resolution breaks.** The `base_dir` is restored to the parent file's directory after the include returns. If the included file's result contains nested `$include` calls (as unevaluated thunks), those calls would resolve relative paths against the *parent's* `base_dir`, not the included file's directory.

3. **Cache coherence breaks.** The cached result must be a fully evaluated value so that all consumers receive semantically equivalent data. A lazy cached thunk could produce different results depending on evaluation context (depth, base_dir at the time of forcing).

Formally: eager materialization is required because the guard set and `base_dir` are stack-scoped (pushed before the call, popped after), but lazy thunks outlive their stack frame. The alternative — extending guard lifetime to match thunk lifetime — would require thunk-to-file provenance tracking that conflicts with tinct's thunk lifecycle model (thunks are anonymous after construction).

This is consistent with Nix's `import` (which also eagerly evaluates the imported expression) and Dhall's imports (which are also strict). In all three systems, the import mechanism is an intentional breach of lazy semantics required by the guard-based cycle detection model.

##### Part 5: Properties

**P1 — Cycle detection termination:** The include recursion terminates for all inputs.

*Argument:* Define include depth as `n = |Σ.guard|`. INCLUDE-EVAL adds exactly one new entry to the guard set before recursing (`canonical ∉ Σ.guard` is a precondition). Each nested include either hits the cache (INCLUDE-HIT, no recursion), detects a cycle (INCLUDE-CYCLE, no recursion), or recurses with `|Σ.guard| = n + 1` (INCLUDE-EVAL). Since `Σ.guard ⊆ {canonical paths on the filesystem}` and the filesystem is finite, `n` is bounded above. Additionally, `d + 1` depth propagation means `MAX_EVAL_DEPTH` provides an independent upper bound on total recursion depth (include + evaluation combined). ∎

**P2 — Cache determinism:** For a fixed filesystem state, `include(path, Σ, d, s)` returns the same result for the same canonical path, regardless of which call site triggered the first evaluation.

*Argument:* INCLUDE-HIT returns `Rc::clone(Σ.cache[canonical])` — a shared pointer to the first evaluation's result. INCLUDE-EVAL evaluates the file exactly once per canonical path (subsequent calls hit the cache). The cached value is `Materialized(v)` (eager), so no further evaluation occurs. The first evaluation is deterministic for a fixed filesystem state (by Property 5 in §Scope Chain Semantics — determinism of the pure subset).

**Failure non-caching:** Failed includes do NOT populate `Σ.cache` — only successful results are cached. A failed `$include("lib.llt")` from call site A does not prevent call site B from re-attempting the same file. Under a fixed filesystem state, the re-attempt produces the same error (determinism holds). Note the two caching levels operate independently: the *include cache* (`Σ.cache`) does not remember failures, but each *call-site thunk* caches its failure permanently via `Failed` state (Semantic Commitment 1). ∎

**P3 — Guard restoration (INCLUDE-RESTORE):** The include guard and `base_dir` are always restored to their pre-call state, even when evaluation fails.

*Correspondence:* `builtins.rs:1129-1135` — the `restore` closure runs in both the `Ok` and `Err` branches of the match on `eval_result`. This ensures that a failed include does not leave stale entries in the guard set (which would cause false cycle-detection errors for subsequent includes of the same file from different call sites).

**Known defect:** The `materialize` call at `builtins.rs:1142` uses the `?` operator, which returns before `restore` runs if materialization fails. This means P3 is violated for materialization errors — the guard entry and modified `base_dir` are not restored. This should be fixed by capturing the materialize result before calling restore: `let val_result = materialize(...); restore(cell); let val = val_result?;`.

**P4 — Include determinism (conditional):** For a fixed filesystem state, the document pipeline `eval_file(file, ρ, d)` is deterministic. When the filesystem changes between evaluations, results may differ — `$include` is the sole source of nondeterminism in tinct (see §Thunk Lifecycle — Semantic Properties, Determinism; also Semantic Commitment 2 in §Thunk Lifecycle — Semantic Commitments).

**P5 — Include isolation:** An included file has no access to the including file's scope chain. Included files evaluate in `Σ.stdlib_env` (builtins + stdlib only), with `$$` initialized to the empty dict:

```
include(path, Σ, d, s):
  eval_file(file, Σ.stdlib_env, d + 1)     ← stdlib env, not caller's env
```

This matches the document isolation property of DOC-PIPELINE (§Scope Chain Semantics Part 2): included files are semantically equivalent to the first document in a standalone file. Data must flow through the include result, not through shared scope:

```lisp
# Namespaced: included file returns a dict, caller accesses its bindings
[utils: [call $include "lib/utils.llt"]
 result: [call $utils.double 21]]

# Merged: included file's dict becomes a parent scope via SEQ-SCOPE
[call $include "lib/utils.llt"]
[result: [call $double 21]]
```

##### Part 6: Implementation Correspondence

| Spec element | Implementation |
|-------------|----------------|
| Σ (IncludeContext) | `builtins.rs:46-55` |
| Thread-local storage | `INCLUDE_CTX` (`builtins.rs:57-59`) |
| set / clear | `set_include_context` / `clear_include_context` (`builtins.rs:65-79`) |
| RESOLVE | `builtins.rs:1044-1058` (resolve + canonicalize) |
| INCLUDE-HIT | `builtins.rs:1061-1063` (cache lookup + Rc::clone) |
| INCLUDE-CYCLE | `builtins.rs:1066-1072` (guard check) |
| INCLUDE-EVAL | `builtins.rs:1074-1161` (read, parse, guard push, eval, materialize, cache) |
| Eager materialization | `builtins.rs:1142` (`materialize(&thunk, None, depth + 1)`) |
| Guard push | `builtins.rs:1110` (`include_guard.borrow_mut().insert`) |
| Guard pop + base_dir restore | `builtins.rs:1129-1135` (`restore` closure) |
| Cache store | `builtins.rs:1147-1153` |
| DOC-PIPELINE (cross-ref) | `eval_file_with_input` (`eval.rs:281-307`) |
| SEQ-SCOPE (cross-ref) | `eval_document` (`eval.rs:199-249`) |

### Type System and Dual-Dispatch Builtins

Several builtins dispatch on their input type (Dict vs Seq), producing different output types depending on the input:

| Builtin | Dict input | Seq input |
|---------|------------|-----------|
| `$map` | Dict (same keys, lazy PendingCall values) | Seq (lazy transform) |
| `$filter` | Seq (must evaluate predicates) | Seq (lazy filter) |
| `$take` | Dict (first n entries by insertion order) | Seq (first n elements) |
| `$drop` | Dict (skip first n entries by insertion order) | Seq (skip first n elements) |
| `$reduce` | Single value (accumulated over entries) | Single value (accumulated over elements) |
| `$join` | String (concatenates values) | String (concatenates elements) |

**Current type system strategy: `Any` for all dual-dispatch builtins.** The type checker (typecheck.rs) assigns type `Any` to these operations. This is the correct choice for now because:

1. **LLT has no union types.** The precise input type would be `Dict | Seq`, which cannot be expressed in the current type system. Without union types, there is no way to accurately represent "accepts either Dict or Seq."

2. **Separate functions would be verbose.** Naming conventions like `$map-dict` and `$map-seq` would work but break the clean, polymorphic API that makes LLT expressive.

3. **Overloaded function types require type system extensions.** True ad-hoc polymorphism (overloading) would require type classes or similar mechanisms, which are not planned for the current phase.

4. **The type checker already handles `Any` uniformly.** Builtins that cannot be precisely typed (e.g., `$from-json`) already use `Any`, and type assertions (`[@Type $expr]`) provide a runtime escape hatch for narrowing back to concrete types.

**Future work:** If the type system gains union types (TODO.md `type-extensions`) or type classes, dual-dispatch builtins could be typed more precisely. Until then, `Any` is the pragmatic choice — it permits all valid uses without introducing false positives.

**`Failed` thunk state:**

To cache evaluation failures instead of restoring `Unevaluated` and re-evaluating on every access attempt:

```
Failed(Box<EvalError>)
```

When a thunk fails to materialize (any state → error), it transitions to `Failed` and stores the error. Future materialization attempts return a clone of the cached error with the `materialization_span` updated to reflect the current access location, preserving the original stack frames. This matches Nix's `nFailed` pattern and prevents quadratic behavior when multiple accesses trigger the same failing computation.

**`PendingBuiltin` preserves laziness:** When the evaluator encounters `[call $builtin ...]`, it does not immediately execute the builtin. Instead, it wraps the builtin name and unevaluated argument thunks in a `PendingBuiltin` state. The builtin executes only when the result is materialized (accessed). This deferred execution is critical for preserving lazy semantics — builtins like `$if` can selectively materialize arguments, and operations like `$map` can return lazy structures without forcing computation.

This completes the laziness picture:

| Thunk state | Represents | Created by |
|-------------|-----------|-----------|
| `Unevaluated` | AST expression + environment | Parser/eval (dict values, fn bodies) |
| `PendingBuiltin` | Deferred builtin call | `[call $builtin ...]` |
| `PendingCall` | Deferred function application | `$map`, `$update`, lazy combinators |
| `InProgress` | Cycle detection sentinel | Materialization |
| `Materialized` | Computed value | After first force |
| `Failed` | Cached evaluation error | Any failed materialization |

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

**Rationale:** The current signature forces all builtins to return fully materialized values, which prevents operations like `$if` from returning the chosen branch as a thunk, and prevents `$map` from returning a dict with lazy PendingCall values. Changing the return type to `Rc<Thunk>` allows builtins to participate in lazy evaluation while maintaining backward compatibility (wrap in `Thunk::new_materialized()` for eager builtins).

**Type inference is unchanged** — return types are determined by unifying the call signature during type checking, not by inspecting returned thunk contents. This change is a runtime optimization only.

**Performance trade-off:** Inherently materializing builtins (~60% of the 28 current builtins: arithmetic, string ops, comparisons) pay two extra heap allocations per call (Thunk + Rc wrapper) to wrap their `Value` result. For lazy-capable builtins (`$if`, `$merge`, `$map`, `$update`), this eliminates the forced materialization boundary. Net benefit when lazy operations dominate. If profiling shows the overhead is significant, a dual-signature approach (`EagerBuiltinFn` vs `LazyBuiltinFn`) could be considered.

### Current vs Planned Laziness Analysis

This table documents every operation's current materialization behavior and the planned improvement in Phase 5 (TODO.md sprint `sequences-and-laziness`). Phase 5 subphases (5a through 5f) are defined at the end of this section.

| Operation | Current Behavior | Planned Behavior | Phase | Rationale |
|-----------|------------------|------------------|-------|-----------|
| **Control Flow** | | | | |
| `$if` | Materializes condition + chosen branch | Return branch thunk directly (no materialization) | 5b | The chosen branch should stay lazy until accessed by caller |
| `$and` | Materializes first arg; second materialized only if first is true (short-circuit via `$if`) | No change (already optimal via lazy `$if`) | — | Short-circuit via `[fn [a b] [call $if $a $b false]]` |
| `$or` | Materializes first arg; second materialized only if first is false (short-circuit via `$if`) | No change (already optimal via lazy `$if`) | — | Short-circuit via `[fn [a b] [call $if $a true $b]]` |
| `$not` | Materializes argument | No change (must inspect value) | — | Inherently materializing |
| `$when`, `$unless` | Materializes condition + chosen body via `$if` | Body returned as thunk (implicit benefit from Phase 5b `$if` change) | 5b (implicit) | Delegates to `$if`; no code change needed |
| `$cond` | Materializes conditions in order + chosen branch via `$if` | Branches returned as thunks (implicit benefit from Phase 5b `$if` change) | 5b (implicit) | Delegates to `$if`; no code change needed |
| **Dict Operations** | | | | |
| `$merge` | Eagerly materializes dict structure to access keys; values remain as thunks (Rc clones) | Lazy overlay: right shadows left, O(1) construction, O(k) access per key for k chained merges. Flattens on iteration. Type is still eagerly inferred. | 5b | Lazy overlay is O(1) construction vs O(n) clone; values stay thunks |
| `$get`, `$get-or` | Structural: returns value thunk | No change (already optimal) | — | Already lazy |
| `$keys` | Structural: keys always evaluated | No change (keys are never thunks) | — | Keys must be known to construct dict |
| `$values` | Structural: returns list of thunks | No change (already optimal) | — | Already lazy |
| `$entries` | Structural: returns list of entry dicts | No change (values stay as thunks) | — | Already lazy |
| `$set`, `$remove` | Structural: add/remove entries (`$set` delegates to `$merge`, inherits its eager cloning) | No change (values stay as thunks; `$set` benefits implicitly from Phase 5b `$merge` improvement) | — | Already lazy on values |
| `$update` | Eagerly applies function to old value | Return dict with PendingCall thunk on updated value | 5e | Defers function application until value accessed |
| `$has?` | Wraps `$try` around access | No change (access is structural) | — | Already optimal |
| `$get-in`, `$get-in-or` | Materializes each step of path | No change (must traverse nested dicts) | — | Inherently materializing to walk path |
| `$length` | Materializes dict to count entries | No change (must count entries) | — | Inherently materializing |
| `$empty?` | Calls `$length` then compares to 0 | No change (inherently materializing) | — | Depends on `$length` |
| **Universal Collection Ops** | | | | |
| `$map` on dict | Eager: builds full result dict via repeated merge (O(n²)), values are call thunks (lazy) | Lazy: return dict with PendingCall thunks, O(n) construct / O(1) per access | 5e | Enables lazy dict transforms |
| `$map` on seq | N/A (Seq not yet implemented) | Lazy: return seq applying function to each element | 5e | Enables infinite sequence transforms |
| `$filter` on dict | Eager: builds full result dict, O(n²) | Return Seq (must evaluate predicates) | 5e | Predicates must run to know which keys to keep |
| `$filter` on seq | N/A (Seq not yet implemented) | Lazy: return seq filtering elements | 5e | Lazy sequence filtering |
| `$reduce`, `$fold` | Materializes accumulator at each step | No change (inherently materializing) | — | Accumulator pattern requires sequential forcing |
| `$map-entries` | Eager: builds full result dict with accumulator pattern, O(n²) | Return dict with PendingCall thunks on transformed entries | 5e | Same as `$map` on dicts |
| `$from-entries` | Eagerly reduces entry pairs into dict via `$merge` | No change (must construct concrete dict) | — | Inherently materializing |
| `$any?`, `$all?` | Short-circuit: materializes elements until condition met/failed | No change (inherently materializing) | — | Predicates must run |
| `$until` | Eagerly iterates until predicate holds | No change (inherently materializing) | — | Must evaluate predicate each step |
| `$find-deep` | Materializes while searching | No change (inherently materializing) | — | Must traverse structure |
| `$flatten` | Eagerly traverses and rebuilds | No change (must inspect values to check if list) | — | Inherently materializing |
| `$zip` | Eagerly builds paired dict | Return lazy Seq for sequences; eager for dicts | 5e | Seq zip is lazy, dict zip is materializing |
| **List Operations** | | | | |
| `$first` | Structural: returns first value thunk | No change (already optimal) | — | Already lazy |
| `$nth`, `$last` | Structural: returns value thunk by position | No change (already optimal) | — | Already lazy |
| `$rest` | Eagerly clones dict minus first entry | Return Seq tail for sequences; O(1) | 5e | Seq `rest` is O(1) vs O(n) dict clone |
| `$cons` | Eagerly clones and shifts all entries | Return Seq cons for sequences; O(1) | 5e | Seq `cons` is O(1) vs O(n) dict clone |
| `$conj` | Materializes + clones dict O(n), inserts new entry O(1) (delegates to `$append` builtin) | No change (acceptable for dicts) | — | O(n) clone + O(1) insert |
| `$concat` | Eagerly clones and merges both lists | Return Seq concat for sequences | 5e | Seq concat is lazy, dict concat is eager |
| `$reverse` | Eagerly builds reversed dict | No change (must know all entries to reverse) | — | Inherently materializing |
| `$reindex` | Eagerly rebuilds with dense 0..n keys | No change (must traverse all entries) | — | Inherently materializing |
| `$sort`, `$sort-by` | Eagerly materializes all values to compare | No change (inherently materializing) | — | Must compare all values to sort |
| `$take` | Positional slice: preserves thunks | Dual-dispatch: Dict preserves keys, Seq returns finite Seq | ✓ 5d | Seq `take` is O(1), dict `take` is structural |
| `$drop` | Positional slice: preserves thunks | Return lazy Seq for sequences | 5e | Seq `drop` is O(1), dict `drop` is structural |
| `$slice` | Positional slice: preserves thunks | No change (already optimal for dicts) | — | Already lazy on values |
| **Sequences** | | | | |
| `$range` | Eager: builds full dict O(n²) | Return lazy Seq, O(1) construction; 1-arg infinite, 2-arg finite | ✓ 5d | Enables infinite ranges |
| `$repeat` | Eager: builds full dict | Return lazy infinite Seq, O(1) construction; 1-arg only | ✓ 5d | Enables infinite repetition |
| `$cycle` | Eager: builds full dict | Return lazy infinite Seq, O(1) construction; 1-arg only | ✓ 5d | Enables infinite cycling |
| `$iterate` | Not yet implemented | Return lazy infinite Seq: `x, f(x), f(f(x)), ...` | ✓ 5d | New lazy sequence constructor |
| `$unfold` | Not yet implemented | Return lazy Seq from step function | ✓ 5d | New lazy sequence constructor |
| `$seq` | Implemented (5c½) | Low-level Seq constructor (cons cell) | 5c½ | Rust builtin for Seq construction |
| `$collect` | Implemented (5c½) | Materialize Seq into dict with integer keys 0..n | 5c½ | Seq → Dict boundary |
| `$head` | Implemented (5c½) | Extract head of Seq (returns thunk, lazy) | 5c½ | Structural Seq operation |
| `$tail` | Implemented (5c½) | Return tail Seq (lazy, does not materialize) | 5c½ | Structural Seq operation |
| `$seq?` | Implemented (5c½) | Type check: returns Bool | 5c½ | Type introspection |
| **Arithmetic & Comparison** | | | | |
| `$+`, `$-`, `$*`, `$/` | Materialize both operands | No change (inherently materializing) | — | Must inspect numeric values |
| `$quot`, `$mod` | Materialize both operands | No change (inherently materializing) | — | Depends on arithmetic |
| `$=`, `$<`, `$>`, `$<=`, `$>=` | Materialize both operands | No change (inherently materializing) | — | Must compare values |
| `$to-int`, `$to-float` | Materialize argument | No change (inherently materializing) | — | Must parse/convert value |
| `$floor`, `$ceil`, `$round`, `$trunc` | Materialize argument | No change (inherently materializing) | — | Must inspect numeric value |
| **Strings** | | | | |
| `$str` | Materialize all arguments | No change (inherently materializing) | — | Must concatenate string content |
| `$split`, `$replace`, `$upper`, `$lower`, `$trim` | Materialize argument | No change (inherently materializing) | — | Must inspect string content |
| `$join` | Materializes separator + all list elements | No change (inherently materializing) | — | Must concatenate all strings |
| `$words` | Materializes string, filters empty | No change (inherently materializing) | — | Depends on `$split` |
| **Composition** | | | | |
| `$apply` | Double-forces: materializes invoke_function result thunk | Return thunk directly | 5b | Current impl materialize+rewrap; should return invoke_function thunk as-is |
| `$identity` | Structural: returns argument thunk | No change (already optimal) | — | Already lazy |
| `$compose` | Structural: returns function thunk | No change (functions are always thunks) | — | Already lazy |
| `$->` (threading) | Structural: threads thunk through functions | No change (already optimal) | — | Already lazy |
| **Runtime & Introspection** | | | | |
| `$eval` | Deep-forces all thunks recursively | No change (inherently materializing by definition) | — | Explicit materialization primitive |
| `$type-of` | Materializes argument to inspect type | No change (inherently materializing) | — | Must know runtime type |
| `$error` | Structural: constructs error value | No change (error is a value) | — | Structural |
| `$try`, `$try-or` | Materializes body, catches exceptions | No change (must run body to catch errors) | — | Inherently materializing |
| `$assert` | Materializes condition | No change (inherently materializing) | — | Must check condition |
| `$from-json` | Materializes JSON string, parses | No change (inherently materializing) | — | Must parse entire JSON |
| `$include` | Evaluates file, returns thunk | Add caching: return cached thunk on re-include | 5f | Jsonnet-style include memoization |
| **Internal (eval.rs)** | | | | |
| `eval_key` (dict construction) | Materializes all dict keys | No change (IndexMap requires concrete keys) | — | Keys must be known for dict insertion |
| `eval_as_dict` (access chains) | Materializes target for access | No change (must know dict structure to access) | — | Inherently materializing to perform access |
| `builtin_keys` | Materializes dict | No change (keys are always evaluated) | — | Keys are never thunks |

**Error reporting impact:** Operations that shift from eager to lazy (e.g., `$if`, `$merge`, `$map`) will report errors at access time rather than construction time. This provides more accurate source locations (pointing to where materialization failed) but changes error timing. Inherently materializing operations continue to produce errors at call time.

**Summary of Phase 5 changes:**
- **5a**: Add `PendingCall` and `Failed` thunk states
- **5b**: Change `BuiltinFn` return type to `Rc<Thunk>`; make `$if`, `$merge`, `$apply` lazier
- **5c**: Add `Value::Seq` and basic Seq builtins (`seq`, `head`, `tail`, `collect`, `seq?`)
- **5d**: Convert `$range`, `$repeat`, `$cycle` to Seq constructors; add `$iterate`, `$unfold`
- **5e**: Make `$map`/`$filter` dual-dispatch (dict vs Seq); make `$update` use PendingCall
- **5f**: Add include caching

---

### Allocation Strategy — Phased Approach

**Decision:** Two-phase strategy. "Phase 1" and "Phase 2" here refer to this section's allocation-specific phases (mapped to TODO.md sprints `perf-foundations` and `iterative-eval`), not the Phase 5/8/9 sprint numbering used elsewhere in this document. Phase 1 applies backward-compatible optimizations to the current `Rc<Thunk>` + `IndexMap<String, Rc<Thunk>>` runtime. Phase 2 introduces arena allocation and flat environments bundled with the iterative evaluator (`iterative-eval`).

**Current allocation profile:**

| Component | Representation | Cost |
|-----------|---------------|------|
| Thunks | `Rc<Thunk>` with `RefCell<ThunkState>` | Individual heap alloc per thunk, triple indirection |
| Environments | `Rc<RefCell<Environment>>` with `IndexMap<String, Rc<Thunk>>` + parent chain | O(depth) variable lookup |
| Dict keys | `Key::String(String)` | Cloned 2× per dict entry (env bindings + dict_map) |
| Thunk origin | `origin: String` | Allocated per thunk, usually empty |

**Phase 1 (perf-foundations):** Backward-compatible optimizations. Baseline: ~113 `Rc::new(Thunk)` calls in eval.rs, ~142 `IndexMap::new()` calls in builtins.rs. Expected impact: 75-85% of addressable allocation cost.

- **Dict literal fast-path** (Nix `maybeThunk`): In `eval_dict`, when `entry.value.node` is `Int|Float|Bool|Str`, create `Materialized` thunks directly instead of wrapping in `Unevaluated`. Eliminates ~40-60% of thunk allocations for config-heavy files. Safe because literals are side-effect-free, deterministic, and don't participate in letrec cycles.
- **String interning**: `HashSet<Rc<str>>` with `Borrow<str>` lookup (avoids key duplication of `HashMap<String, Rc<str>>`). Interns *structural identifiers only* — `Key::String`, variable names, builtin names, and thunk origins. Does NOT intern user data strings (may be large and unique). Reduces key cloning to `Rc::clone` and enables O(1) pointer-equality comparison. Scoped to evaluation session lifetime (lives in `EvalContext`, cleared per `eval_file()`). Production alternative: `lasso::Rodeo` for zero-copy Spur handles.
- **Key cloning reduction**: Eliminate the 2× `String` clone per dict entry in `eval_dict` (once into `dict_env` bindings, once into `dict_map`). Use `entry_mut()` pattern or restructure insert order. ~30% of dict allocation cost.
- **AST cloning reduction**: Change `CallExpr` args from `Spanned<Expr>` to `Rc<Spanned<Expr>>` so `eval_call` can `Rc::clone` instead of deep-cloning entire AST subtrees per argument. ~20-40% of call overhead. Internal refactor to ast.rs and parser.rs; backward-compatible at the public API level.
- **func_label allocation reduction**: `format!("${name}")` on every PendingCall creation → `Cow<'static, str>` for the common VarRef case (most calls). Only allocate for DotAccess labels. ~5-10% of call overhead.
- **Capacity hints**: `IndexMap::with_capacity(entries.len())` on all dict construction paths (`eval_dict`, `builtin_drop` Dict path, range access, `builtin_split`).
- **SmallVec**: `SmallVec<[Rc<Thunk>; 4]>` for call args (most calls have ≤4 args), `SmallVec<[StackFrame; 8]>` for error stacks.
- **Origin optimization**: `origin: String` → `Rc<str>` via string interner, with static empty sentinel for the common case.

**Phase 2 (iterative-eval):** Arena allocation + flat environments, bundled with the recursive-to-iterative evaluator conversion.

- **Arena allocator**: Replace `Rc<Thunk>` with arena-allocated thunks. Recommended approach: index-based arena (`Vec<Thunk>` + `ThunkId` newtype over `usize`) for stable references, bounds-checked indexing, and safe letrec (allocate `ThunkId` slots, fill later, no UB). Alternatives (typed-arena, bumpalo) require unsafe and don't offer clear wins for LLT's use case.
- **Flat environments with slot indices**: Replace `IndexMap<String, Rc<Thunk>>` chain with flat `Vec` arrays indexed by compile-time (level, slot) pairs (de Bruijn levels). Variable lookup becomes O(1). Environment reuse in function calls becomes trivially safe (each call writes to its own activation frame).
- **Variable resolution pass**: Pre-eval pass assigns (level, slot) indices to every `VarRef`. This pass also enables TCO detection.

**Arena lifetime and persistent values:** The arena lifetime is **one document section** — the text between `---` boundaries (or the entire file for single-section documents). At each `---` boundary, values reachable from the section result are **selectively migrated** from the arena to `Rc`-backed persistent storage, bound as `$$` for the next section, and the section's arena is dropped.

**Selective migration** is a scoped copying pass that preserves thunk state — it translates storage, not evaluation state. Unevaluated thunks stay unevaluated (lazy), Materialized thunks keep their cached values, closures retain their environment chains. The `---` boundary is **not** a strictness point. This preserves the existing lazy pipeline semantics (§Scope Chain Semantics, DOC-PIPELINE): the `---` boundary does not force evaluation.

The migration algorithm traces from `$$` (the section result) and rewrites arena handles to `Rc`-backed storage:

```
migrate(value, arena, thunk_table, env_table) → Rc<Thunk>:
  for each ThunkId in value:
    if thunk_table[id] exists:     return thunk_table[id]  (preserves sharing)
    thunk = arena[id]
    rc = Rc::new(Thunk::placeholder())       (allocate before recursing)
    thunk_table[id] = rc                     (insert before recursing — breaks cycles)
    rc.fill(match thunk.state:
      Materialized(v)            → Materialized(migrate_value(v, arena, thunk_table, env_table))
      Unevaluated(expr, env)     → Unevaluated(expr, migrate_env(env, arena, thunk_table, env_table))
      PendingBuiltin(f, args, …) → PendingBuiltin(f, migrate_args(args, …), …)
      PendingCall(f_θ, args, …)  → PendingCall(migrate(f_θ, …), migrate_args(…), …)
      Failed(e)                  → Failed(e)
      InProgress                 → unreachable at --- boundary
    )
  return rc
```

Two-phase allocation: `Rc::new(placeholder())` is inserted into the table *before* recursing into the thunk's state. This is the standard graph-copying pattern for structures with cycles — letrec environments contain mutual references, so the table entry must exist before `migrate_env` encounters the same ThunkId transitively. The placeholder is filled via `RefCell` after the recursive migration completes. This matches how `deep_materialize` inserts into its visited set before recursing.

**Two translation tables** preserve identity across the migration boundary:

- `thunk_table: HashMap<ThunkId, Rc<Thunk>>` — ensures two references to the same arena thunk map to the same `Rc<Thunk>`.
- `env_table: HashMap<EnvId, Rc<RefCell<Environment>>>` — ensures two closures capturing the same arena environment share the same migrated environment. Without this, letrec groups that share an environment would become independent copies, breaking the sharing invariant.

AST nodes (`Rc<Spanned<Expr>>`) are reference-counted and arena-independent — they are shared, not copied. The builtins environment (root of every parent chain) is always `Rc`-backed and never arena-allocated — it is the base case that terminates `migrate_env` recursion.

Within a section, all thunks are arena-allocated and lazy. Letrec entries reference each other freely within the arena. At `---`, only thunks reachable from `$$` are migrated — unreachable intermediate thunks (temporaries, shadowed bindings) are reclaimed when the arena drops.

**What migrates correctly:**

| Value type | Migration behavior |
|------------|-------------------|
| Primitives (Int, Str, Bool, …) | Copied directly (no arena handles) |
| Dict entries | Each thunk migrated; sharing preserved via table |
| Functions/closures | Captured environment chain migrated recursively |
| Infinite Seq | Only the cons cell is migrated; lazy tail stays lazy |
| `$include` results | Already Rc-backed (include cache outlives sections) |

Per execution context:

| Context | Arena lifetime | Cross-boundary value | Notes |
|---------|---------------|---------------------|-------|
| CLI (single section) | Entire eval | None | One arena, dropped at end. No migration. |
| CLI (multi-section) | Per section | `$$` (selectively migrated) | Arena per section, migrate at `---` |
| REPL | Per input | `$$` (selectively migrated) | Each input is implicitly a section |
| LSP | Per section | `$$` (selectively migrated) | Editing section N re-evaluates N+ with cached `$$` from N-1 |

**Cost model:** Migration is O(thunks reachable from `$$`), not O(total section thunks). For sections where `$$` is a small result derived from large intermediate computations, migration cost is much lower than deep-materialization. For sections where most thunks are reachable from `$$`, cost approaches deep-materialization minus the forcing cost (migration copies state; deep-materialization evaluates).

**Rejected alternatives:** (1) Session-scoped arena — unbounded memory growth during long REPL sessions; requires stop-the-world compaction with pointer fixup across all live references. (2) Hybrid arena+Rc — two allocation paths; every thunk creation must decide arena vs Rc; closures capturing thunks make escape analysis intractable. (3) Deep-materialization at `---` — changes language semantics (lazy→eager), breaks closures (env chains hold dangling arena handles after drop), and diverges on infinite sequences in `$$`. (4) Per-eval copy-out without section granularity — forces materialization of intermediate values within a section, losing laziness benefits.

**LSP incremental re-evaluation:** Migrated `$$` values are self-contained `Rc`-backed storage with no arena references. The LSP caches `$$` per section. Editing section N re-uses cached `$$` from section N-1 (already migrated, no re-evaluation) and re-evaluates only sections N through the end.

**`$include` interaction:** Included files are evaluated in their own arena. The include cache stores migrated results — the cache outlives any single section's arena. An `$include` call returns an already-migrated `Rc`-backed value, which is arena-independent and can be used freely across sections. This creates a controlled one-way dependency within sections: arena-allocated thunks may reference `Rc`-backed `$include` results, but never the reverse. This is structurally determined (section-local = arena, imported = Rc) and does not require per-thunk escape analysis — the "hybrid arena+Rc" alternative (rejected above) fails because it requires per-thunk decisions, not because mixing storage backends is inherently unsound.

**Rationale:** The iterative evaluator is already planned and shares prerequisites with arena allocation — both require explicit frame management and compile-time analysis. Bundling avoids two separate invasive refactors. Phase 1 captures 75-85% of addressable allocation wins with near-zero risk. Profiling data from Phase 1 guides whether Phase 2's arena is necessary.

**Measurement plan:** Phase 1 must establish baseline metrics before and after optimization: total allocations per eval (count `Rc::new`, `IndexMap::new`, `Vec::new`), peak memory usage (heaptrack RSS on dict-heavy and deeply-nested workloads), and allocation hotspots (which paths account for >10% of allocations). Decision threshold for Phase 2: if Phase 1 achieves >80% allocation reduction, defer Phase 2 indefinitely; if <50%, proceed.

**Key tradeoff:** Environment lookup stays O(depth) until Phase 2, but string interning makes each lookup step cheaper (pointer comparison vs byte comparison), and the literal fast-path reduces total thunk allocations significantly.

**Precedent:** Nix uses flat `Value*[]` arrays with de Bruijn levels and Boehm GC. Jsonnet uses GC heap with flat bindings. Nickel uses `Rc<RefCell<Closure>>` (same as LLT's current approach). Phase 1 keeps LLT at Nickel's level; Phase 2 moves toward Nix's level.

**Constraint:** Phase 2's arena model must handle letrec self-reference safely in Rust (thunk slots allocated before fill, no dangling pointers). Research into safe Rust arena patterns (typed-arena, bumpalo, or index-based arenas with `Vec<Thunk>` + `ThunkId` handles) is required before Phase 2 implementation. Study how Rust projects (salsa, rustc's `ty::TyCtxt`, cranelift) solve arena + interior references.

### Iterative Evaluator — Defunctionalized CPS (CEK Machine)

**Decision:** Replace the recursive `eval()` / `materialize()` call stack with an iterative CEK machine (Control-Environment-Kontinuation). Continuations are defunctionalized — each closure that CPS would create becomes a variant in a `Cont` enum, stored in a `Vec<Cont>` stack.

**Problem:** `eval()` and `materialize()` are mutually recursive across 8+ call patterns. Deeply-nested lazy chains exhaust the Rust call stack before `MAX_EVAL_DEPTH` fires. LLT works around this with a 64MB worker thread stack.

**Architecture:** Two enums, one loop.

`Action` represents what to do now (the "control" register):

```rust
enum Action {
    Eval { expr: Rc<Spanned<Expr>>, env: Rc<RefCell<Environment>>, depth: usize },
    Materialize { thunk: Rc<Thunk>, mat_span: Option<Span>, depth: usize },
    Continue(Result<Value, Box<EvalError>>),
}
```

`Cont` represents what to do with the result (the reified continuation / "kontinuation" stack):

```rust
enum Cont {
    // eval() continuations — access chains
    DotAccessForce { field: String, span: Span, depth: usize },
    BracketForceTarget { key_expr: Rc<Spanned<Expr>>, env: ..., span: Span, depth: usize },
    BracketForceKey { target: Value, span: Span },
    RangeForceTarget { start_expr: ..., end_expr: ..., env: ..., span: Span, depth: usize },
    RangeForceStart { target: Value, end_expr: ..., env: ..., span: Span, depth: usize },
    RangeForceEnd { target: Value, start: Value, span: Span },

    // eval() continuations — calls and type assertions
    CallForceFunc { args: Box<Vec<Rc<Thunk>>>, named: Box<IndexMap<...>>, env: ..., span: Span, depth: usize, label: String },
    TypeAssertCheck { annotation: ..., env: ..., span: Span, depth: usize },
    TypeAssertForce { type_expr: ..., default_expr: Option<...>, env: ..., span: Span, depth: usize },

    // eval() continuations — dict construction
    DictBuildKey { value_expr: Rc<Spanned<Expr>>, remaining: ..., env: ..., span: Span, depth: usize },

    // eval() continuations — function defaults
    BindArgDefault { param: String, remaining_params: ..., env: ..., depth: usize },

    // materialize() continuations
    Memoize { thunk: Rc<Thunk>, mat_span: Option<Span>, origin: String },
    PendingBuiltinForceResult { thunk: Rc<Thunk>, mat_span: Option<Span>, ... },
    PendingCallForceFunc { thunk: Rc<Thunk>, args: Box<Vec<Rc<Thunk>>>, call_span: Span, ... },
    PendingCallForceResult { thunk: Rc<Thunk>, mat_span: Option<Span>, ... },

    // Document pipeline
    DocumentScope { remaining: Vec<Spanned<Expr>>, env: ..., depth: usize },

    // Deep materialization
    DeepEntries { map: Rc<IndexMap<Key, Rc<Thunk>>>, idx: usize, ... },
    DeepSeqTail { tail: Rc<Thunk>, ... },
}
```

Large fields in `CallForceFunc` and `PendingCallForceFunc` are boxed to keep the `Cont` enum ≤96 bytes. `DeepEntries` holds an `Rc` to the original map plus an index rather than cloning entries into a `Vec`.

The main loop is a two-register machine — `action` (what's happening now) and `stack` (what's waiting):

```rust
fn run(initial: Action) -> Result<Value, Box<EvalError>> {
    let mut stack: Vec<Cont> = Vec::with_capacity(64);
    let mut action = initial;

    loop {
        action = match action {
            Action::Eval { expr, env, depth } => {
                match &expr.node {
                    Expr::Int(n) => Action::Continue(Ok(Value::Int(*n))),
                    Expr::DotAccess { expr, field } => {
                        stack.push(Cont::DotAccessForce { field, span, depth });
                        Action::Eval { expr, env, depth }
                    }
                    // ...
                }
            }
            Action::Materialize { thunk, mat_span, depth } => {
                match /* thunk state */ {
                    Materialized(v) => Action::Continue(Ok(v.clone())),
                    Unevaluated { expr, env } => {
                        stack.push(Cont::Memoize { thunk, mat_span, origin });
                        Action::Eval { expr, env, depth: depth + 1 }
                    }
                    // ...
                }
            }
            Action::Continue(result) => {
                match stack.pop() {
                    None => return result,
                    Some(cont) => /* dispatch on cont, produce next Action */
                }
            }
        };
    }
}
```

**How this works:** Instead of recursive calls, each continuation point becomes a `Cont` variant pushed onto the stack. When a sub-computation completes (`Action::Continue`), the top continuation is popped and dispatched. The `Cont` variant stores exactly the state that a closure would have captured — no more, no less.

**Memoize error handling:** On `Err`, `Cont::Memoize` must call `cache_failure()` (set `ThunkState::Failed`) before propagating the error up the continuation stack. This ensures failed thunks cache their error and don't retry on every access.

**Builtin return dispatch:** Builtins return `Rc<Thunk>`, not `Value`. After a builtin call, the CEK machine inspects the result: if the thunk is already `Materialized`, extract the value and produce `Action::Continue(Ok(value))`. If it is `Unevaluated` or `PendingBuiltin`, the dispatch depends on the **continuation context**, not a dynamic inference:

- If the top of the continuation stack is `Cont::Memoize` (the builtin was called during materialization of a parent thunk), the result must be materialized — produce `Action::Materialize { thunk: result_thunk, ... }`.
- If the top is `Cont::DictBuildValue`, `Cont::BindArgDefault`, or similar construction contexts, the result stays lazy — produce `Action::Continue(Ok(Value::from_thunk(result_thunk)))`.

This is **structurally determined** by the `Cont` variant on the stack, not inferred at runtime. Each `Cont` variant statically knows whether it needs a materialized value or accepts a thunk. The strictness signature table (§Selective Materialization — Formal Specification) declares per-argument strictness for builtin *inputs*; the continuation context determines strictness for builtin *outputs*. Builtins like `$if` and `$get` return lazy thunks that must not be auto-materialized when used as dict values or function arguments.

**deep_materialize:** Not a separate recursive function — it is expressed as `DeepEntries` and `DeepSeqTail` continuations within the same CEK loop. No separate recursive helper.

**Tail-call optimization:** In tail position (e.g., last expression in a function body), set `action = Action::Eval { body, ... }` without pushing a `Cont`. The current frame is reused. TCO for recursive stdlib functions (`fold`, `map`, `filter`) follows the same pattern: detect tail calls during the variable resolution pass, mark them, and skip the continuation push. TCO applies to user-defined function calls only. Builtin calls always push a continuation — builtins rely on `PendingBuiltin` thunk deferral for lazy behavior, not tail-call elimination.

**Error stack traces:** Walk `Vec<Cont>` to reconstruct the call stack. Each `Cont::CallForceFunc` carries the call-site span and label, replacing the current `EvalError::stack` vector. This gives precise "materialized at" context for every frame in the stack.

**Cont variant count:** ~18-20 variants, one per continuation point in the current recursive evaluator. Each variant stores only its specific continuation data (Rc pointers + Span + small fields). Target frame size: ≤96 bytes per Cont (achieved by boxing large fields in the biggest variants).

**Relationship to perf-foundations:** This design is Phase 2 of the allocation strategy. Arena allocation and flat environments integrate naturally: `Cont` variants hold `ThunkId` handles into the arena, and the `Vec<Cont>` stack's lifetime defines the arena's lifetime scope.

**Precedent:** Jsonnet's VM uses 22 `FrameKind` variants with a value register (production-tested at Google). Nickel uses an iterative stack machine with `OpFirst`/`OpSecond` continuations (production Rust). Both are defunctionalized CPS machines. The theoretical foundation is Felleisen & Friedman's CEK machine.

**Recursive call sites being converted:**

| Current recursive call | Becomes |
|----------------------|---------|
| `eval()` → `eval()` (TypeAssert, desugar, defaults) | `Action::Eval` + `Cont::TypeAssertCheck` etc. |
| `eval_call()` → `eval()` + `materialize()` | `Action::Eval` + `Cont::CallForceFunc` |
| `eval_dot_access()` → `eval()` + `materialize()` | `Action::Eval` + `Cont::DotAccessForce` |
| `eval_bracket_access()` → `eval()` + `materialize()` ×2 | `Action::Eval` + `Cont::BracketForceTarget` → `Cont::BracketForceKey` |
| `eval_range_access()` → `materialize()` ×3 | `Action::Materialize` + `Cont::RangeForceTarget` → `Cont::RangeForceStart` → `Cont::RangeForceEnd` |
| `eval_dict()` → computed key materialization | `Action::Eval` + `Cont::DictBuildKey` |
| `eval_document()` → `eval()` + `materialize()` | `Action::Eval` + `Cont::DocumentScope` (`$$` bound as `Unevaluated` thunk, never materialized) |
| `bind_args_thunks()` → default eval | `Action::Eval` + `Cont::BindArgDefault` |
| `materialize()` → `eval()` + `materialize()` | `Action::Eval` + `Cont::Memoize` |
| `materialize()` → builtin call + `materialize()` | Builtin dispatch + `Cont::PendingBuiltinForceResult` |
| `materialize()` → `materialize()` (PendingCall) | `Action::Materialize` + `Cont::PendingCallForceFunc` → `Cont::PendingCallForceResult` |
| `deep_materialize()` → `materialize()` + recurse | `Action::Materialize` + `Cont::DeepEntries` / `Cont::DeepSeqTail` (within CEK loop, no separate helper) |

---

### Row-Variable Unification — Kinded Rémy Model (Dict+Tail Representation)

**Not yet implemented — approved design.**

Replace the current closed-strict/open-lenient record unification with kinded row-variable unification following Rémy (1994). Row variables become first-class participants in type inference with a separate **Row kind**, enabling the type checker to infer record extension and restriction through polymorphic function boundaries. The design omits Rémy's presence/absence flags (tinct has no typed field deletion) but preserves the kind separation that makes the soundness proof clean and leaves the door open for full Rémy if typed field deletion is needed later.

**Representation choice:** The Row type uses a **dict+tail** representation (field map plus tail variable) rather than Rémy's cons-list (`Extend(l, τ, ρ)`). Rémy's left-commutativity equations (`l₁:τ₁ ; l₂:τ₂ ; ρ ≡ l₂:τ₂ ; l₁:τ₁ ; ρ`) make rows semantically unordered — the dict+tail representation computes directly in the quotient algebra of rows under these equations, representing each equivalence class as a single canonical form (unordered field map) rather than an arbitrary representative (ordered cons-list). This eliminates the need for a field extraction operation during unification and prevents duplicate labels structurally (the map enforces unique keys). Both representations encode the same abstract algebra; the choice is operational, not theoretical. Bernstein (2024) uses this representation; PureScript and Elm use similar approaches internally.

**Design rationale:** Rémy (1994) Theorem 4.7 proves principal type existence for the kinded row system. The kind separation prevents the class of soundness bugs found in Elm (issue #656, open since 2015) where row variables and type variables are conflated. Wand (1987, Theorem 1, corrected 1988) proves completeness for the presence-only restriction (no absence flags), which is a subsystem of Rémy's full system. PureScript demonstrates that kinded rows work at production scale. Nickel (Rust-based config language) validates kinded row polymorphism in a Rust codebase similar to tinct's.

#### Part 1: Row Kind

**Notation:** This section uses ρ for row variables, following Rémy (1994) and Wand (1987). The §Scope Chain Semantics section uses ρ for environments, following Launchbury (1993). The two uses are confined to separate sections and do not interact — the row-variable ρ participates in type inference, while the environment ρ participates in evaluation.

Rows are a **separate sort** from types. A row maps labels to types with an optional tail variable:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowTail {
    Empty,              // closed row — no more fields
    RowVar(String),     // ρ — row variable (bindable in substitution)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub fields: IndexMap<String, Type>,   // known fields {l₁: τ₁, l₂: τ₂, ...}
    pub tail: RowTail,                    // Empty (closed) or RowVar(ρ) (open)
}
```

The `Type` enum changes to reference `Row`:

```rust
pub enum Type {
    // ... existing variants unchanged ...
    Record(Row),   // was Record(IndexMap<String, Type>, RowRest)
    // ...
}
```

**Kind grammar:**

```
κ ::= Type                  # kind of types (Int, String, Record(ρ), ...)
    | Row                   # kind of rows ({x: Int, ...ρ}, {}, ...)
```

Row variables have kind `Row`; type variables have kind `Type`. The substitution enforces this: `type_map: HashMap<String, Type>` and `row_map: HashMap<String, Row>` are separate maps. A type variable can never be bound to a row, and a row variable can never be bound to a type — this invariant is structural (enforced by Rust's type system), not checked at runtime.

**Uniqueness invariant.** The `IndexMap<String, Type>` structurally prevents duplicate labels — each label maps to exactly one type. In Rémy's full system, this property is maintained by the presence/absence discipline (each label appears once, flagged as Present or Absent). The dict+tail representation achieves the same invariant through the map data structure. This eliminates the class of bugs where cons-list extraction leaves duplicate labels in row remainders.

**Relationship to evaluation.** The `Row` type exists only in the type system (`types.rs`, `typecheck.rs`). The evaluator continues to use `IndexMap<Key, Rc<Thunk>>` for runtime dicts. There is no `Row` at runtime — the type-level row is erased during type checking. This separation is standard: PureScript and OCaml both use different representations for type-level rows and runtime records.

**Forward compatibility with full Rémy.** If typed field deletion is needed in the future, the field map gains presence flags:

```rust
// Future extension — not implemented now
pub enum FieldPresence { Present, Absent }

pub struct Row {
    pub fields: IndexMap<String, (FieldPresence, Type)>,  // l → (pre(τ) | abs)
    pub tail: RowTail,
}
```

The current design is a strict subset — every field entry `l: τ` is implicitly `l: (Present, τ)`. Adding the flag later requires updating field access patterns but not the unification algorithm structure. The extract-and-recurse flow gains a presence-compatibility check (Present must match Present), but the overall partitioning and tail-binding logic is preserved.

#### Part 2: Substitution and Occurs Check

The substitution splits into two kinded maps:

```rust
pub struct Substitution {
    pub type_map: HashMap<String, Type>,   // α → τ  (kind: Type)
    pub row_map: HashMap<String, Row>,     // ρ → r  (kind: Row)
}
```

**Application** (`apply`) walks types and rows, replacing bound variables from the appropriate map:

```
apply_type(τ, S):
  TypeVar(α)      → if α ∈ S.type_map then apply_type(S.type_map[α], S) else TypeVar(α)
  Record(r)       → Record(apply_row(r, S))
  Function(ps, r) → Function(map(apply_type, ps), apply_type(r, S))
  Seq(τ)          → Seq(apply_type(τ, S))
  otherwise       → τ  (Int, Float, Str, Bool, Number, Any, literals)

apply_row(Row { fields, tail }, S):
  fields' = { l: apply_type(τ, S) for (l, τ) in fields }
  match tail:
    Empty       → Row { fields: fields', tail: Empty }
    RowVar(ρ)   → if ρ ∈ S.row_map:
                     let bound = apply_row(S.row_map[ρ], S)
                     Row { fields: fields' ∪ bound.fields, tail: bound.tail }
                   else:
                     Row { fields: fields', tail: RowVar(ρ) }
```

Note: when splicing a bound row variable, the bound row's fields are merged into the current field map. Since both maps enforce unique keys and the bound row was produced by unification (which partitions fields), there are no duplicate labels to resolve. If a duplicate key were to arise (implementation bug), it would indicate a constraint violation — fail with an internal error rather than silently overwriting.

**Occurs check** is per-kind:

```
type_var_occurs(α, τ):
  TypeVar(β)        → α == β
  Record(r)         → type_var_occurs_in_row(α, r)
  Function(ps, r)   → any(type_var_occurs(α, p) for p in ps) ∨ type_var_occurs(α, r)
  Seq(τ)            → type_var_occurs(α, τ)
  otherwise         → false

row_var_occurs(ρ, Row { fields, tail }):
  (any(row_var_occurs_in_type(ρ, τ) for τ in fields.values()))
  ∨ match tail:
      RowVar(σ) → ρ == σ
      Empty     → false

row_var_occurs_in_type(ρ, τ):
  Record(r)       → row_var_occurs(ρ, r)
  Function(ps, r) → any(row_var_occurs_in_type(ρ, p) for p in ps)
                     ∨ row_var_occurs_in_type(ρ, r)
  Seq(τ)          → row_var_occurs_in_type(ρ, τ)
  otherwise       → false

type_var_occurs_in_row(α, Row { fields, tail }):
  any(type_var_occurs(α, τ) for τ in fields.values())
  # tail is a RowVar or Empty — neither contains type variables
```

The row-variable occurs check traverses **both** the tail (preventing direct infinite rows like `ρ = {x: Int, ...ρ}`) **and** field types (preventing infinite types through nesting like `ρ = {x: Record({y: Int, ...ρ})}` — where binding ρ to this row would create an infinite structure). This is necessary because `Record(Row)` embeds a row inside a type, so a row variable can appear transitively inside a field type via nesting.

#### Part 3: Row Unification

Row unification is the core of the design. It uses **field partitioning** — given two rows, partition their fields into shared (present in both) and unique (present in only one), then unify shared field types and bind row variable tails to the other side's unique fields. This directly computes in the quotient algebra of Rémy's left-commutativity equations.

**Unification algorithm:**

```
unify_rows(Row { fields: F₁, tail: t₁ }, Row { fields: F₂, tail: t₂ }, S):
  # Step 1: Resolve bound row variables
  (F₁, t₁) = resolve_row(F₁, t₁, S)
  (F₂, t₂) = resolve_row(F₂, t₂, S)

  # Step 2: Partition fields
  shared  = F₁.keys() ∩ F₂.keys()
  unique₁ = { l: F₁[l] for l in F₁.keys() \ shared }
  unique₂ = { l: F₂[l] for l in F₂.keys() \ shared }

  # Step 3: Unify shared field types
  for l in shared:
    S = unify_types(F₁[l], F₂[l], S)

  # Step 4: Unify remainders (unique fields + tails)
  S = unify_remainders(unique₁, t₁, unique₂, t₂, S)
  return S

resolve_row(fields, tail, S):
  match tail:
    RowVar(ρ) if ρ ∈ S.row_map →
      let bound = apply_row(S.row_map[ρ], S)
      return (fields ∪ bound.fields, bound.tail)
    _ → return (fields, tail)
```

**Remainder unification** handles the four cases from Wand (1987):

```
unify_remainders(U₁, t₁, U₂, t₂, S):
  # Note: Case 4 must be matched before Cases 2/3 in implementation
  # to prevent pattern shadowing (Case 2 is strictly more general than Case 4).
  match (U₁.is_empty(), t₁, U₂.is_empty(), t₂):

    # Case 1: No unique fields on either side — unify tails directly
    (true, _, true, _) →
      unify_tails(t₁, t₂, S)

    # Case 4: Both have unique fields — create fresh row variable for shared tail
    # Precondition: ρ₁ ≠ ρ₂ (same-variable case handled by Case 1 after apply)
    # Occurs check prevents infinite rows: if ρ₁ appears in U₂'s field types
    # (e.g., ρ₁ = {x: Record({y: Int, ...ρ₁}), ...ρ_fresh}), binding ρ₁ would
    # create an infinite structure. On failure: emit a type error "infinite row
    # type: ρ₁ occurs in its own binding", halt unification.
    (false, RowVar(ρ₁), false, RowVar(ρ₂)) →
      let ρ_fresh = fresh row variable
      if row_var_occurs(ρ₁, Row(U₂, RowVar(ρ_fresh))): ERROR infinite row
      if row_var_occurs(ρ₂, Row(U₁, RowVar(ρ_fresh))): ERROR infinite row
      S ∪ {ρ₁ → Row { fields: U₂, tail: RowVar(ρ_fresh) }}
        ∪ {ρ₂ → Row { fields: U₁, tail: RowVar(ρ_fresh) }}

    # Case 2: Only left has unique fields — right tail must absorb them
    (false, _, _, RowVar(ρ₂)) →
      if row_var_occurs(ρ₂, Row(U₁, t₁)): ERROR infinite row
      S ∪ {ρ₂ → Row { fields: U₁, tail: t₁ }}

    # Case 3: Only right has unique fields — left tail must absorb them
    (_, RowVar(ρ₁), false, _) →
      if row_var_occurs(ρ₁, Row(U₂, t₂)): ERROR infinite row
      S ∪ {ρ₁ → Row { fields: U₂, tail: t₂ }}

    # Error cases: closed tail cannot absorb unique fields from the other side
    (false, _, _, Empty) → ERROR: extra fields {U₁.keys()} in closed row
    (_, Empty, false, _) → ERROR: extra fields {U₂.keys()} in closed row

unify_tails(t₁, t₂, S):
  match (t₁, t₂):
    (Empty, Empty)           → S
    (RowVar(ρ₁), RowVar(ρ₂)) →
      if ρ₁ == ρ₂: S
      else: S ∪ {ρ₁ → Row { fields: {}, tail: RowVar(ρ₂) }}
    (RowVar(ρ), Empty)       → S ∪ {ρ → Row { fields: {}, tail: Empty }}
    (Empty, RowVar(ρ))       → S ∪ {ρ → Row { fields: {}, tail: Empty }}
```

**Case 4** is the key insight from Wand (1987): when both rows have unique fields and open tails, a fresh row variable `ρ_fresh` is created to represent the (yet unknown) fields shared by both tails. Each original tail is bound to the other side's unique fields plus this shared unknown. This correctly propagates constraints — if either tail is later unified with a concrete row, the constraints flow through `ρ_fresh` to the other side. Case 4 must be matched before Cases 2/3 in implementation because Case 2's pattern `(false, _, _, RowVar(ρ₂))` is strictly more general and would shadow Case 4, incorrectly binding only one tail instead of both.

**Type-level unification for records:**

```
UNIFY-RECORD:
  unify_types(Record(r₁), Record(r₂), S) = unify_rows(r₁, r₂, S)
```

All record unification delegates to row unification. The current nine-case `match` in `unify()` for Record (lines 319-340 of types.rs) is replaced by this single delegation.

**Complexity:** Field partitioning is O(n) where n is the total number of fields across both rows (hash-based set operations on IndexMap keys). This improves on the cons-list extract-and-recurse approach which is O(n²) worst case (O(n) scan per field). For tinct's use case (configuration records, typically < 100 fields) both are acceptable, but O(n) is strictly better.

#### Part 4: Instantiation and Generalization

Row variables participate in generalization and instantiation via the standard HM mechanism, extended to two kinds.

**Variable collection** (two sets):

```
collect_type_vars(τ) → Set<String>     # type variables in τ
collect_row_vars(τ) → Set<String>      # row variables in τ

collect_row_vars(Record(Row { fields, tail })):
  row_vars_in_fields(fields) ∪ row_vars_in_tail(tail)

row_vars_in_fields(fields) = ⋃{ collect_row_vars(τ) for τ in fields.values() }
row_vars_in_tail(RowVar(r)) = {r}
row_vars_in_tail(Empty)     = {}
```

**Instantiation** freshens both namespaces independently:

```
instantiate(τ, counter):
  type_vars = collect_type_vars(τ)
  row_vars = collect_row_vars(τ)
  renaming = Substitution::new()
  for α in type_vars:
    renaming.type_map[α] = TypeVar(fresh_name(counter))
  for ρ in row_vars:
    renaming.row_map[ρ] = Row { fields: {}, tail: RowVar(fresh_name(counter)) }
  return apply_type(τ, renaming)
```

Row variables and type variables use **separate namespaces** — `_t0` is unambiguously a type variable or a row variable depending on which map it appears in. The current implementation conflates them in a single `BTreeSet<String>` and single `Substitution::map` — this must be split.

**Generalization** (with levels, per §Let-Generalization): row variables carry levels identically to type variables. A row variable `ρ` with `levels[ρ] > ℓ` is generalized at a let-binding. The `TypeScheme` representation extends to track both:

```rust
pub struct TypeScheme {
    pub type_vars: Vec<String>,    // universally quantified type variables
    pub row_vars: Vec<String>,     // universally quantified row variables
    pub ty: Type,
}
```

**Dependency note:** Row-variable generalization requires levels-based let-generalization (§Let-Generalization), which is not yet implemented. The initial implementation should treat all row variables as unquantified (matching current behavior where type variables in dict fields are not generalized). Row-variable generalization can be added when let-generalization lands.

#### Part 5: Access Chain Constraint Generation

With row variables bindable, access chains can generate constraints instead of falling back to `Any` (resolving the limitation documented in §Access Chain Evaluation Part 5).

```
check_dot_access(Γ, e, field) :
  τ = infer(Γ, e)
  τ' = apply_subst(τ)
  match τ':
    Record(Row { fields, tail }) →
      if field ∈ fields: return fields[field]
      else match tail:
        RowVar(ρ) → let β = fresh_type_var()
                     let ρ_fresh = fresh_row_var()
                     unify_tails(RowVar(ρ), RowVar(ρ_fresh))  # not needed, just bind:
                     S ∪ {ρ → Row { fields: {field: β}, tail: RowVar(ρ_fresh) }}
                     return β
        Empty     → ERROR: field not found in closed record
    TypeVar(α)  → let β = fresh_type_var()
                   let ρ = fresh_row_var()
                   unify(TypeVar(α), Record(Row { fields: {field: β}, tail: RowVar(ρ) }))
                   return β
    Any         → Any
    _           → ERROR: not a record
```

The TypeVar case is new and important: `$x.name` where `$x` has unknown type `α` generates the constraint `α = Record({name: β, ...ρ})`, binding `α` to a record type with at least field `name`. Multiple accesses like `$x.name` and `$x.age` accumulate constraints naturally — the first binds `α` to `Record({name: β₁, ...ρ₁})`, the second extracts from `RowVar(ρ₁)` and binds `ρ₁` to `Row({age: β₂, ...ρ₂})`, resulting in `α = Record({name: β₁, age: β₂, ...ρ₂})`.

The RowVar case in Record access binds `ρ` to `Row({field: β}, RowVar(ρ_fresh))`, correctly recording the constraint "ρ must contain field with type β, plus whatever else is in ρ_fresh." This is sound because if ρ is later unified with a row that lacks the field, the binding will conflict.

**Implementation note:** Part 5 is a new capability, not required for the core migration. It can be implemented after the Row type and unification are working, as a separate enhancement to the type checker.

#### Part 6: Subtyping

`is_subtype` handles `Record(Row)` directly using the field map:

```
is_subtype(Record(Row { fields: F₁, tail: t₁ }), Record(Row { fields: F₂, tail: t₂ })):
  # All fields in sup must be present in sub with subtype field types
  for (l, τ_sup) in F₂:
    τ_sub = F₁[l] or return false
    if not is_subtype(τ_sub, τ_sup): return false

  # Closed sup requires sub has no extra fields
  match t₂:
    Empty     → F₁.keys() ⊆ F₂.keys()
    RowVar(_) → true    # open via row var — extra fields allowed
```

This preserves the current behavior (§Type Inference Algorithm S-REC) while working with the new Row representation. The `RowVar` in subtyping position acts as `Open` — consistent with the gradual typing design where unknown row extensions are permitted.

#### Part 7: Display

Row types display using tinct's existing syntax:

```
Display for Row { fields, tail }:
  field_strs = ["{l}: {τ}" for (l, τ) in fields]
  tail_str = match tail:
    Empty     → None
    RowVar(r) → Some(if r.starts_with("_") then "..." else "...{r}")
  parts = field_strs ++ [tail_str].flatten()
  return parts.join("  ")
```

Generated row variable names (from anonymous `...` syntax) are displayed as bare `...` rather than `..._r0` to avoid confusing users with names they didn't write. Named row variables (user-written `...name`) display as `...name`.

Examples:
- `Record(Row { fields: {name: Str, age: Int}, tail: Empty })` → `[name: Str  age: Int]`
- `Record(Row { fields: {name: Str}, tail: RowVar("r") })` → `[name: Str ...r]`
- `Record(Row { fields: {name: Str, age: Int}, tail: RowVar("_r0") })` → `[name: Str  age: Int ...]`
- `Record(Row { fields: {}, tail: Empty })` → `[]`
- `Record(Row { fields: {}, tail: RowVar("rest") })` → `[...rest]`

#### Part 8: Migration from Current Representation

The migration replaces `RowRest` with `RowTail`, adds `Row` as a struct, and changes `Record(IndexMap, RowRest)` to `Record(Row)`:

| Current | New |
|---------|-----|
| `RowRest::Closed` | `RowTail::Empty` |
| `RowRest::Open` | `RowTail::RowVar(fresh)` (anonymous open becomes named) |
| `RowRest::RowVar(name)` | `RowTail::RowVar(name)` |
| `Record(fields, rest)` | `Record(Row { fields, tail })` |
| `Substitution { map }` | `Substitution { type_map, row_map }` |
| `collect_type_vars` (single set) | `collect_type_vars` + `collect_row_vars` (two sets) |

**`RowRest::Open` elimination.** Anonymous open records (`[name: Str ...]`) become `Record(Row { fields: {name: Str}, tail: RowVar(fresh) })` — the type checker generates a fresh row variable name when resolving `Expr::Rest(None)`. The parser produces `Expr::Rest(None)` as today; the type checker owns the fresh-name counter and generates `_r{n}` names during type resolution. This makes all openness explicit and eliminates the `Open` variant entirely.

**Structural similarity.** The dict+tail representation is structurally close to the current `Record(IndexMap<String, Type>, RowRest)` — the field map is preserved as-is, and `RowRest` becomes `RowTail` with `Closed` → `Empty` and `Open` eliminated. This minimizes the migration surface compared to a cons-list representation. Pattern matches on `Record(fields, rest)` become `Record(Row { fields, tail })` — a mechanical transformation.

**Substitution split.** Every `subst.map.insert(name, ty)` must be routed to the correct map: type variables to `type_map`, row variables to `row_map`. In the current implementation, row variables and type variables share a single namespace. After the split, the unification function determines which map to use based on the variable's kind (inferred from context: `TypeVar(α)` → `type_map`, `RowTail::RowVar(ρ)` → `row_map`).

**Helper functions needed:**

```rust
impl Row {
    fn closed(fields: IndexMap<String, Type>) -> Row { ... }
    fn open(fields: IndexMap<String, Type>, var: String) -> Row { ... }
    fn empty() -> Row { ... }
    fn var(name: String) -> Row { ... }  // Row { fields: {}, tail: RowVar(name) }
}
```

#### Part 9: Properties

**P1 — Principal types.** Every well-typed expression has a principal type under the kinded row unification algorithm. For the presence-only restriction (no absence flags), this follows from Wand (1987, Theorem 1, corrected 1988). The full system with presence/absence flags is covered by Rémy (1994, Theorem 4.7). The dict+tail representation computes in the quotient algebra of Rémy's rows under left-commutativity; since it is isomorphic to the cons-list representation, the principal type theorem applies unchanged.

**P2 — Kind safety.** Type variables and row variables inhabit separate namespaces enforced by the `Substitution` structure (`type_map` vs `row_map`) and by Rust's type system (`Type` vs `Row` are distinct types). A type variable α can never be bound to a row, and a row variable ρ can never be bound to a type. This prevents the class of bugs exemplified by Elm issue #656.

**P3 — Row commutativity.** `{a: Int, b: Str, ...ρ}` unifies with `{b: Str, a: Int, ...ρ}` — field order in rows is irrelevant. This is enforced structurally by the dict+tail representation: the `IndexMap` is an unordered (by semantics) field collection, so commutativity is automatic rather than computed via extraction.

**P4 — Occurs check termination.** The per-kind occurs check prevents infinite types (`α = Record({x: α})`) and infinite rows (`ρ = {x: Int, ...ρ}`). The row-variable occurs check traverses field types to prevent infinite structures through nesting (`ρ = {x: Record({y: Int, ...ρ})}`). Combined with the finite-depth property of tinct's AST, unification terminates.

**P5 — Backward compatibility.** All currently well-typed programs remain well-typed. The migration changes internal representation but not the type language visible to users. Programs that previously inferred `Any` for row-polymorphic positions will now infer more precise types — this is strictly more informative, not breaking.

**P6 — Forward compatibility with full Rémy.** Adding presence/absence flags changes field map values from `Type` to `(FieldPresence, Type)`. The partitioning algorithm gains a presence-compatibility check (Present must match Present, Absent must match Absent), and field access must skip Absent fields. The overall structure (partition shared/unique, unify shared, bind tails) is preserved. See Part 1: Row Kind for the extension point.

**P7 — Label uniqueness.** The `IndexMap<String, Type>` structurally prevents duplicate labels in any row. This invariant is maintained through all operations: construction (from source), unification (partitioning preserves uniqueness), and substitution application (field merging of disjoint maps). No runtime duplicate-label check is needed.

**P8 — Tail-field disjointness.** The fields of a row and the fields of its resolved tail are always disjoint, by construction from the partitioning step of unification. When `unify_remainders` binds a tail `ρ` to `Row { fields: U, tail: t }`, the unique fields `U` were computed as the set difference `F_other \ shared` — fields present in the other row but not in the row containing `ρ`. Since `ρ` is the tail of the row that contributed the `shared` fields, and `U` contains only fields *not* in that row, the two sets are disjoint. This guarantees that `apply_row` field merging (which unions a row's explicit fields with its resolved tail's fields) never encounters duplicate keys.

#### Part 10: Formal References

- **Rémy, D. (1994).** "Type inference for records in natural extension of ML." In *Theoretical Aspects of Object-Oriented Programming*, pp. 291–346. MIT Press. — Principal type theorem (Theorem 4.7), kinded row unification, presence/absence flags, left-commutativity equations. Foundational model for tinct's kind separation between Type and Row.
- **Wand, M. (1987).** "Complete type inference for simple objects." In *LICS '87*, pp. 37–44. IEEE. — Row variables as record tails, completeness proof (corrected 1988). Proves principal types for the presence-only restriction. Tinct's four-case unification algorithm follows Wand's field-partitioning structure with Rémy's kind separation.
- **Gaster, B.R. & Jones, M.P. (1996).** "A polymorphic type system for extensible records and variants." TR NOTTCS-TR-96-3, Nottingham. — Lacks predicates for field absence. Not adopted (requires type class infrastructure) but relevant if typed field deletion is added or if `$merge` needs precise open-record typing.
- **Harper, R. & Pierce, B. (1991).** "A record calculus based on symmetric concatenation." In *POPL '91*, pp. 131–142. ACM. — Concatenation typing with disjointness constraints. Relevant to `$merge` formal specification.
- **Bernstein, M. (2024).** "Adding row polymorphism to Damas-Hindley-Milner." Blog post. — Tutorial implementation of Wand's approach in dict-based (quotient algebra) representation. Reference implementation for the four-case field-partitioning unification pattern used in tinct's design.

---

## Open Questions / TODO

Design questions that still need to be resolved. All other design questions have been resolved and appear in the Confirmed Decisions section above.

### Structural Contracts

- [ ] **Shape/contract system** — Predicate-based validation separate from the type system. Allows runtime constraints beyond what types express (e.g., "port must be 1-65535").
- [ ] **OpenAPI integration** — Load external schemas as contracts for validation.
- [ ] **Lazy vs eager validation** — Validate on materialization vs explicit `[call $validate! $schema $data]`?

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
│   Source    │  .llt file (documents separated by ---)
└──────┬──────┘
       │
       ▼
┌─────────────┐
│   Parser    │  Text → AST (File > Document > Expr)
└──────┬──────┘
       │
       ▼
┌─────────────┐
│  Type Check │  Infer & verify types
└──────┬──────┘
       │
       ▼
┌─────────────┐
│  Evaluator  │  Per-document: scope chains, $$ pipeline, lazy
└──────┬──────┘
       │
       ▼
┌─────────────┐
│     CLI     │  Input parsing, $eval, output serialization
└─────────────┘
```

> **Note:** The type checker (TODO.md Phase 2a/2b: `theoretical-foundations` / `type-extensions`) runs after parsing but type errors are advisory — evaluation proceeds regardless of type errors. This matches the design philosophy that types aid development without blocking execution.

### EvalContext — Evaluation Infrastructure Context

The evaluator threads an `EvalContext` through `eval()`, `materialize()`, and builtin dispatch. This separates evaluation infrastructure (file resolution, sandboxing) from variable bindings (`Environment`) and stack depth tracking (`depth`).

**Design decision:** EvalContext replaces the thread-local `INCLUDE_CTX` pattern. Thread-locals create invisible coupling, prevent multi-file LSP support (each document needs its own include context), and require fragile set/clear ceremonies at every call site.

**Config/State split:** EvalContext separates immutable session configuration from mutable evaluation state. Config is `Rc` (no RefCell) — the compiler enforces immutability. State is `Rc<RefCell>` for interior mutability.

```rust
struct EvalConfig {
    base_dir: PathBuf,
    stdlib_env: Rc<RefCell<Environment>>,
    // future: sandbox_policy, max_depth_override, trace_enabled
}

struct EvalState {
    include_guard: HashSet<PathBuf>,
    include_cache: HashMap<PathBuf, Rc<Thunk>>,
    // future: trace_log, eval_stats
}

struct EvalContext {
    config: Rc<EvalConfig>,         // shared, immutable
    state: Rc<RefCell<EvalState>>,   // shared, mutable
}
```

**What stays separate:**
- `depth: usize` — stack-depth counter, passed by value and incremented per recursive call (`eval(expr, env, ctx, depth + 1)`). Not session state — it's naturally fork-friendly for parallel evaluation paths.
- `Environment` — variable bindings and lexical scope chain. Created and nested per scope.

**Key invariant:** EvalContext is evaluation-session infrastructure; Environment is lexical scoping; depth is call-stack tracking. A single EvalContext is shared across the entire evaluation of a file, while Environments are created per scope and depth increments per recursive call.

**Threading pattern:** `Rc<RefCell<EvalContext>>` — same pattern as `Environment`. Thunks capture `Rc::clone(&ctx)` at creation time and use it at materialization time. This is necessary because thunks are deferred (`Unevaluated`, `PendingBuiltin`, `PendingCall`) and materialized in a different stack frame than where they were created. `&mut EvalContext` would cause borrow conflicts with lazy evaluation.

**ThunkState captures EvalContext:** `Unevaluated`, `PendingBuiltin`, and `PendingCall` all store `ctx: Rc<RefCell<EvalContext>>` alongside their existing `env: Rc<RefCell<Environment>>`. When a thunk is forced, it uses the captured context for include resolution, sandboxing, etc.

**BuiltinArgs:** Gains a `ctx: Rc<RefCell<EvalContext>>` field. The existing `depth: usize` field remains (call-site depth, captured at PendingBuiltin creation time). Most builtins ignore ctx; only `$include` and future I/O builtins use it.

**Public API:** `EvalContext`, `EvalConfig`, and `EvalState` are public. Callers construct an EvalContext and pass it to `eval_file()`. The `set_include_context()` / `clear_include_context()` functions are removed — the fragile set/clear ceremony is replaced by straightforward parameter passing.

**Per-caller patterns:**
- **CLI (main.rs):** Constructs EvalContext from CLI args (file path → base_dir), passes to eval_file.
- **LSP:** Each DocumentState gets its own EvalContext. DocumentStore extracts base_dir from document URI. Config (stdlib_env) is shared across documents; state is per-document.
- **REPL:** Fresh EvalContext per eval_input() call. Session env persists (accumulates bindings), but include state resets per input. Config (stdlib_env, base_dir) is shared via Rc across commands.

**Precedent:** Nix's `EvalState`, Nickel's `VirtualMachine`, Dhall's normalization context. Standard pattern in mature language implementations for separating evaluation infrastructure from variable bindings.

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
[a b key: val c]                # OK — positional and named freely interleaved

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

## Formal References

Foundational papers grounding tinct's design decisions. Each citation identifies the formal model a subsystem corresponds to and the guarantees it provides.

**Type inference:**
- Damas, L. & Milner, R. (1982). Principal type-schemes for functional programs. In *POPL '82*, pp. 207–212. ACM. — Proves existence of principal types for Hindley-Milner. tinct uses levels-based let-generalization (Kiselyov 2013) with annotation-driven polymorphism. Principal types hold for the annotated subset; full principality requires all parameters to be annotated.
- Robinson, J.A. (1965). A machine-oriented logic based on the resolution principle. *JACM*, 12(1), 23–41. — The unification algorithm at the core of `unify()` in `src/types.rs`. Robinson is purely syntactic (no subtyping); tinct extends it with [U-SUBSUME], a ground-type compatibility check using the subtype lattice. This is a pragmatic middle ground — full subtyping integration would require algebraic subtyping (Dolan & Mycroft 2017).
- Dolan, S. & Mycroft, A. (2017). Polymorphism, subtyping, and type inference in MLsub. In *POPL '17*, pp. 228–242. ACM. — Introduces algebraic subtyping: a principled combination of ML-style parametric polymorphism with subtyping that preserves principal types. Uses polar types (input vs output) and extends unification to handle subtyping constraints directly. tinct intentionally does not adopt this — [U-SUBSUME] is a simpler ground-type compatibility check that avoids MLsub's complexity while covering tinct's literal-type subtyping needs. See `doc/whatif/algebraic.md` for analysis of what adoption would require.
- Pierce, B.C. & Turner, D.N. (2000). Local type inference. *ACM TOPLAS*, 22(1), 1–44. — Foundational paper for bidirectional type checking. tinct's type checker uses synthesis (⇒) and checking (⇐) modes, with subsumption bridging subtyping and inference. `check_expr` uses `is_subtype` for concrete types and `unify` for types with variables.
- Dunfield, J. & Krishnaswami, N.R. (2021). Bidirectional typing. *ACM Computing Surveys*, 54(5), 1–38. — Comprehensive survey of bidirectional typing. Establishes that bidirectional checking is the standard approach for combining HM inference with subtyping. tinct's design follows the "synthesize then subsume" pattern for checking positions.
- Dunfield, J. & Pfenning, F. (2004). Tridirectional typechecking. In *POPL '04*, pp. 281–292. ACM. — Extends bidirectional typing to refinement types. Relevant because tinct's singleton literal types (`IntLiteral(42)`, `StringLiteral("hello")`) are subtypes of their base types (`Int`, `Str`) — a simpler form of the type refinements D&P handles. The subsumption rule [SUB] mediates between singleton and base types; [U-SUBSUME] provides confluence within unification.
- Kiselyov, O. (2013). How OCaml type checker works — or what polymorphism and garbage collection have in common. — Levels-based approach to let-generalization used by tinct. Type variables carry integer levels; generalization quantifies variables whose level exceeds the enclosing scope.

**Row polymorphism:**
- Rémy, D. (1994). Type inference for records in natural extension of ML. In *Theoretical Aspects of Object-Oriented Programming*, pp. 291–346. MIT Press. — Proves decidable inference with row variables. tinct's row types (open records with `...`, named row variables with `...rest`) follow Rémy's approach. Full row-variable binding in `unify()` is planned but not yet implemented — current row unification is structural but does not bind row variables to remainders.

**Coinductive types and productivity:**
- Coquand, T. (1994). Infinite objects in type theory. In *Types for Proofs and Programs*, pp. 62–78. Springer. — Foundational treatment of coinductive types in type theory. Introduces the guardedness condition for productive corecursion. tinct's `Value::Seq` is a coinductive cons-cell; productivity is ensured pragmatically (productive-by-construction combinators + depth limit) rather than statically, since Coquand's proof requires totality.
- Turner, D.A. (2004). Total functional programming. *J. Universal Computer Science*, 10(7), 751–768. — Argues for eliminating partiality via a data/codata distinction. tinct deliberately retains general recursion (Turing-completeness) at the cost of static productivity guarantees, following Haskell/Nix rather than Dhall/Turner.
- Abel, A. & Pientka, B. (2013). Wellfounded recursion with copatterns: a unified approach to termination and productivity. In *ICFP '13*, pp. 185–196. ACM. — Unifies termination and productivity checking via sized types and copatterns. Cited as incompatible with HM inference, motivating tinct's pragmatic approach.
- Abel, A. (2012). Type-based termination, inflationary fixed-points, and mixed inductive-coinductive types. In *FICS '12*, EPTCS 77, pp. 1–11. — Sized types for Agda's termination/productivity checker. Alternative to syntactic guardedness; requires constraint solving beyond unification.

**Strictness analysis:**
- Mycroft, A. (1981). Abstract interpretation and optimising transformations for applicative programs. Ph.D. thesis, University of Edinburgh. — Introduces per-argument strictness annotations for higher-order functional programs via abstract interpretation. tinct's strictness signature table (§Selective Materialization — Formal Specification) uses Mycroft's S/L classification to declare which builtin arguments are forced.

**Call convention:**
- Garrigue, J. (1995). Labeled and optional arguments for Objective Caml. In *JSSST Workshop*, pp. 1–14. — Formalizes labeled and optional function arguments with separate default evaluation environments. tinct's `default_env` parameter in `bind_args_thunks` (§Call Convention — Formal Specification) follows Garrigue's insight that the environment for evaluating defaults must be a parameter, not hard-coded — normal calls use the caller's environment, `$apply` uses the closure environment. tinct's Kotlin-model naming (any parameter is nameable) goes beyond Garrigue's labeled-only approach — see §Call Convention Part 1 C-NAMED-VALID.

**Evaluation semantics:**
- Plotkin, G.D. (1981). A structural approach to operational semantics. Tech. Rep. DAIMI FN-19, Aarhus University. — Foundational framework for structural operational semantics (SOS). tinct's delta rules for builtin materialization behavior (§Selective Materialization) follow Plotkin's style of inference rules with premises and conclusions.
- Ariola, Z.M. & Felleisen, M. (1997). The call-by-need lambda calculus. *J. Functional Programming*, 7(3), 265–301. — Equational theory for call-by-need evaluation. Proves confluence (diamond property) for the pure call-by-need calculus. tinct's pure subset (no `$include`) satisfies this property; `$include` introduces evaluation-order dependence that breaks confluence (see §Thunk Lifecycle — Semantic Commitments).
- Launchbury, J. (1993). A natural semantics for lazy evaluation. In *POPL '93*, pp. 144–154. ACM. — The formal semantics for call-by-need evaluation. tinct's thunk lifecycle (Unevaluated → InProgress → Materialized) is a faithful implementation of Launchbury's natural semantics, extended with PendingBuiltin, PendingCall, and Failed states for deferred computation and error caching. The scope chain semantics (§Scope Chain Semantics — Formal Specification) uses Launchbury's heap model for letrec environment construction and variable lookup. See §Thunk Lifecycle — Formal Specification for the complete state transition DAG and proof obligations.
- Nakata, K. & Hasegawa, M. (2009). Small-step and big-step semantics for call-by-need. *J. Functional Programming*, 19(6), 699–722. — Extends Launchbury's natural semantics to handle cyclic references in letrec, proving that blackholing (InProgress detection) ensures termination for all terms. tinct's cycle detection via the `InProgress` thunk state is exactly Nakata & Hasegawa's blackhole mechanism. The scope chain semantics (§Scope Chain Semantics — Property 2) uses this result to prove mutual visibility terminates. tinct extends their model with error memoization (`Failed` state), which they do not address.
- Peyton Jones, S.L., Reid, A., Henderson, F., Haskell, C.A. & Sestoft, P. (1999). A semantics for imprecise exceptions. In *PLDI '99*, pp. 25–36. ACM. — Formalizes exception semantics in a lazy language. tinct's `Failed` thunk state memoizes errors permanently (Semantic Commitment 1 in §Thunk Lifecycle), matching the deterministic subset of Peyton Jones et al.'s semantics — tinct has no non-deterministic exception selection since errors are purely deterministic.

**Abstract machines:**
- Danvy, O. & Nielsen, L.R. (2003). Defunctionalization at work. In *PPDP '03*, pp. 162–174. ACM. — Systematic defunctionalization from higher-order to first-order programs. tinct's PendingBuiltin and PendingCall thunk states are defunctionalized continuations in the sense of Reynolds (1972). The planned iterative evaluator (see Iterative Evaluator section) makes this CEK machine correspondence explicit, following Felleisen & Friedman (1986).

**Desugaring:**
- Pombrio, J. & Krishnamurthi, S. (2014). Resugaring: lifting evaluation sequences through syntactic sugar. In *PLDI '14*, pp. 361–371. ACM. — Formalizes how to map desugared evaluation steps back to surface syntax for error reporting. Motivates the universal practice of desugaring before evaluation. tinct's `$_` desugaring follows this pipeline: parse → desugar → typecheck → eval.
- Krishnamurthi, S. (2012). *Programming Languages: Application and Interpretation (PLAI)*. — Textbook pipeline: parse → desugar → typecheck → evaluate. Desugaring produces a core language AST that all downstream passes consume. tinct's `$_` transformation is a desugaring in this sense.

**Parsing:**
- Ford, B. (2004). Parsing expression grammars: a recognition-based syntactic foundation. In *POPL '04*, pp. 111–122. ACM. — Proves O(n) parsing with packrat memoization. tinct's pest grammar (`src/grammar.pest`) implements a PEG with ordered choice, no left recursion, and finite lookahead.

## Resources

- [Crafting Interpreters](https://craftinginterpreters.com/) — evaluator implementation
- [Write You a Haskell](http://dev.stephendiehl.com/fun/) — lazy evaluation
- [pest.rs](https://pest.rs/) — PEG parser (used for `src/grammar.pest`)
