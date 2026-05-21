# What If: Unified Syntax Reform for tinct

**State:** Accepted — 2026-05-01

What would it take to adopt bare-word references, implied call, `$`
disambiguation, and `%` pipeline naming as a single coherent syntax
reform?

## Current State

tinct's syntax rests on three pillars:

1. **`$` sigil for every reference.** Bare words are string literals;
   `$word` is a variable reference (doc/02-syntax.md §Variable References).
   Every reference costs one `$` character.

2. **Explicit `call` keyword.** `[call $f $x]` for every function
   application (doc/01-introduction.md §Principle 3). Every call costs one `call`
   keyword.

3. **Anonymous `$$` pipeline.** Documents separated by `---` pass
   data through `$$` (doc/09-documents.md §Document Structure, DOC-PIPELINE).
   Sections cannot be named or referenced individually.

```tinct
# Current tinct
[call $collect
  [call $take 10
    [call $filter [fn [n] [call $= 0 [call $mod $n 2]]]
      [call $range 0]]]]
---
[call $map [fn [entry]
  [call $merge $entry [processed: true]]] $$]
```

### What's Missing

1. **Conventional syntax.** Most languages (Python, Nix, Haskell,
   Rust, Jsonnet) use bare identifiers for references. The `$` sigil
   is unusual outside of shell scripting.

2. **Concise function application.** `call` adds 4 characters and
   one token per application. Nested calls accumulate significant
   noise.

3. **Pipeline naming.** Multi-section documents cannot name
   intermediate results. Complex pipelines thread everything through
   a single `$$`, making multi-input composition impossible.

4. **LLM generation overhead.** Both `$` and `call` are constant
   sources of LLM generation errors — forgetting `$` or omitting
   `call` are the two most common tinct mistakes.

## Why Unified Syntax Reform Matters for tinct

These changes are deeply interrelated. Bare-word references and
implied call were initially analyzed as separate features and
appeared "mutually exclusive in their simplest forms." This
proposal resolves that tension by inverting the `$` heuristic:
instead of `$` in head position meaning "call," `$` in head
position means "NOT a call" — a data escape hatch. This inversion
unifies all four features under a coherent sigil model.

1. **Conventional syntax.** Bare-word references align tinct with
   the broader language ecosystem. Developers and LLMs generate
   correct code with less effort.

2. **Lisp-ergonomic calls.** `[f x y]` approaches `(f x y)`. tinct
   gains Lisp's concision while retaining dict syntax.

3. **Named pipeline sections.** `%config`, `%defaults`, `%output` —
   sections become individually referenceable, enabling DAG-structured
   document pipelines instead of linear chains.

4. **Dramatic token reduction.** Removing `$` from every reference
   and `call` from every application reduces token count by ~30-40%
   in functional tinct code.

5. **Coherent sigil roles.** `$` has one clear purpose:
   - Bare word = variable reference (the common case, no sigil)
   - `$` = position-dependent disambiguator (rare — computed keys,
     data-sequence heads)
   - `%` prefix = pipeline naming convention (not enforced by the
     language — `%` is a valid identifier character)

## Design

This proposal combines bare-word references, implied call, the
inverted `$` heuristic, and `%` pipeline naming into a single
coherent design.

### Bare-Word References

Adopt the Nix/Jsonnet model: bare words in value position are
variable references. Strings must be quoted. Keys remain strings.

| Position | Current | Proposed | Example |
|----------|---------|----------|---------|
| Value (after `:`) | String literal | Variable reference | `name: x` → ref `x` |
| Unkeyed entry | String literal | Variable reference | `[a b c]` → refs |
| Key (before `:`) | String key | String key (unchanged) | `name:` → key "name" |
| Quoted | String literal | String literal (unchanged) | `"hello"` → string |

Revised literal recognition precedence:

1. Numeric pattern → Int or Float literal (unchanged)
2. `true`/`false` → Bool literal (unchanged)
3. Quoted string `"..."` → String literal (unchanged)
4. **Everything else → variable reference** (changed from string)

> **Note on `null`:** Tinct has no null value (see `doc/03-data-model.md §No Null`). Under the
> new syntax, the bare word `null` becomes a variable reference — it will produce an
> undefined-variable error at runtime unless `null` is explicitly bound in scope. Use `[]`
> (the empty dict) as the idiomatic tinct no-value placeholder.

```tinct
[
  name: "Alice"              # key "name", value string "Alice"
  greeting: [str "Hello " name]  # refs to str and name
  env: "production"          # must quote strings now
  port: 8080                 # numbers unchanged
  debug: true                # booleans unchanged
]
```

### Implied Call

If the first unkeyed element of a `[]` expression is a bare word
(reference), the expression is a function call. The bare word is
the function; remaining entries are arguments.

**Bracket interpretation rules (priority order):**

| Priority | Condition | Interpretation | Example |
|----------|-----------|----------------|---------|
| 1 | Empty brackets | Empty dict | `[]` |
| 2 | Keyword in head | Special form | `[fn ...]`, `[call ...]` |
| 3 | First entry is keyed | Dict | `[name: x ...]` |
| 4 | Bare word in head | Call | `[f x y]` |
| 5 | `$`-prefixed head | Data (not a call) | `[$f x y]` |
| 6 | Literal in head | Data | `[1 2 3]`, `["a" "b"]` |

```tinct
# Rule 2: keywords (unchanged — call remains valid)
[call f x]             # explicit call, still works
[fn [x] [* x 2]]      # function definition
[type Int]             # type annotation

# Rule 3: keyed head → dict
[name: "Alice"  age: 30]   # dict

# Rule 4: bare word head → call
[f x y]                # call f(x, y)
[map [fn [x] [* x 2]] data]   # nested implied calls
[merge base overlay]   # call merge(base, overlay)
[f x name: "val"]      # call with named argument

# Rule 5: $ head → data (not a call)
[$f x y]               # sequence: [ref(f), ref(x), ref(y)]
[$f]                   # single-element sequence: [ref(f)]

# Rule 6: literal head → data
[1 2 3]                # list of integers
["a" "b" "c"]          # list of strings
[true false true]      # list of booleans
```

`call` remains a valid keyword. Both forms produce identical AST:

```tinct
[call map double data]     # explicit
[map double data]          # implied
```

The `call` keyword is required when the function is a computed
expression rather than a bare identifier — the bracket
interpretation rules only recognize bare words in head position:

```tinct
[call [get-handler request] data]       # function from another call
[call handlers[request.type] request]   # function from bracket access
[call % data]                           # pipeline value is a function
```

`call` is also available for documentation clarity and backwards
compatibility during migration.

**Zero-argument calls.** `[f]` is a zero-argument call to `f`,
matching every Lisp: `(f)` is always application. To construct a
single-element data sequence containing a reference, use `[$f]`.

**Single-element bracket expressions** — all four cases:

| Expression | Interpretation | Rule |
|-----------|----------------|------|
| `[f]` | Call: `f()` | Priority 4: bare word in head |
| `[$f]` | Data: `[ref(f)]` | Priority 5: `$`-prefixed head |
| `["s"]` | Data: `["s"]` | Priority 6: literal in head |
| `[42]` | Data: `[42]` | Priority 6: literal in head |

In configuration, `stages: [deploy]` calls `deploy()` with zero arguments.
Use `stages: [$deploy]` for a single-element sequence containing a reference,
or `stages: ["deploy"]` for a single-element sequence containing a string.

**Reserved words.** `fn`, `type`, and `call` are contextual reserved words —
they match priority 2 in head position and cannot be used as call targets via
implied call. They remain valid as dict keys (`fn: something`) and in
non-head value positions.

**Priority table lookahead.** Priorities 3 and 5 require one token of
lookahead past the head element to check for `:` (distinguishing a keyed
entry from a call or data head). A PEG implements this as ordered alternatives
with a lookahead predicate; the iterative parser peeks one token ahead before
committing to a frame type.

### `$` as Position-Dependent Disambiguator

`$` is repurposed from a universal reference sigil to a position-
dependent override: "switch from the default interpretation at this
position." It appears only where disambiguation is needed — a
small fraction of current usage.

| Position | Default | `$` overrides to | Example |
|----------|---------|-------------------|---------|
| Key (before `:`) | String key | Computed key (reference) | `$key: val` |
| Head (first in `[]`) | Call target | Data entry (not a call) | `[$f x y]` |
| Other value | Reference | Reference (redundant, harmless) | `[f $x y]` ≡ `[f x y]` |

**Key position** — unchanged from current tinct:

```tinct
key: "host"
[$key: "localhost"]     # computed key: resolves key → "host"
[host: "localhost"]     # string key: literal "host"
```

**Head position** — new. `$` on the first element prevents call
interpretation. The bracket is treated as data:

```tinct
[f x y]              # call: f(x, y)
[$f x y]             # data: sequence [ref(f), ref(x), ref(y)]
```

Only the head element needs `$`. Subsequent elements are
interpreted normally (bare words = references):

```tinct
stages: [$parse transform format]
# data: [ref(parse), ref(transform), ref(format)]
# only $parse carries the disambiguator
```

**Other positions** — `$` on a non-head, non-key bare word is
redundant. `$x` and `x` resolve to the same reference. `$` is
permanently valid on references in value position; the formatter
normalizes `$x` to `x`, but both forms are always accepted.

**No ambiguity between key and head positions.** The `:` after a
token is what distinguishes key context from head context.
`$key:` (followed by `:`) = computed key. `$key` (followed by
whitespace or another token) = data head.

### Data Sequences with `$` Escape

When constructing a dict with references as positional entries,
`$` on the head element prevents call interpretation:

```tinct
stages: [$parse transform format]
# data: [ref(parse), ref(transform), ref(format)]
```

Only the head needs `$` — subsequent entries are already in
value position (bare words = references). The `$` on the first
element may look asymmetric, but it communicates intent: "this
bracket is data, not a call."

Note: `$seq` is the *builtin function* that constructs lazy
`Value::Seq` cons cells (`[call $seq head tail]`). It is not
a keyword and serves a different purpose — lazy generator
sequences, not dict construction.

### `%` Pipeline Variable and Section Naming

Replace `$$` with `%`. The `%` prefix is a naming convention, not
a language-enforced sigil — `%` is a valid identifier character,
so `%foo` is a regular variable name like any other. Users are
free to use `%`-prefixed names for non-pipeline purposes. The
convention exists to make pipeline data visually distinct.

**Anonymous pipeline** — `%` refers to the previous section's
output, like `$$` today:

```tinct
[host: "localhost"  port: 8080]
---
[merge % [tls: true]]
---
[deploy %]
```

**Named sections** — `---` lines can include a `%name` to bind
the section's output:

```tinct
--- %defaults
[host: "localhost"  port: 8080]

--- %overrides
[host: "prod.example.com"  tls: true]

---
[merge %defaults %overrides]
```

`---` is a section header — it introduces the section that
follows, not the one that preceded it. Named sections bind their
output as `%name` in all subsequent sections. `%` always refers
to the immediately previous section's output, whether that
section was named or not. Duplicate section names within a file
are a parse error. A bare `%` with no following identifier on a
section header (i.e., `--- %` followed by whitespace or end of
line) is also a parse error — it would ambiguously rebind the
anonymous pipeline variable.

The first section may omit its `---` header (no name, no
pragmas), or include one:

```tinct
--- %config
[host: "localhost"]

---
[deploy %config]
```

**Formal semantics** — extends DOC-PIPELINE (doc/09-documents.md §Document
Structure):

```
Σ₀ = {}                                     (named-section map)
θ₀ = input_thunk

∀j ∈ 1..m:
  pipeline_bindings = {% ↦ θⱼ₋₁}
                    ∪ {%n ↦ Σⱼ₋₁(n) | n ∈ dom(Σⱼ₋₁)}
  ρ_docⱼ = (pipeline_bindings, Some(ρ_base))
  θⱼ = eval_document(docⱼ.exprs, ρ_docⱼ, d)
  Σⱼ = if docⱼ.name = Some(n)
       then Σⱼ₋₁[n ↦ θⱼ]
       else Σⱼ₋₁

────────────────────────────────────────────
eval_file(documents, ρ_base, input_thunk, d) ⇒ θₘ
```

Documents remain isolated — `ρ_docⱼ` inherits only from `ρ_base`
(builtins), not from prior documents' scope chains. Data flows
through pipeline bindings (`%`, `%name`), not the scope chain.
The pipeline is lazy: `θⱼ₋₁` is passed without materialization.

**Lexer representation** — `%` is added to the set of valid
identifier characters. `%` and `%foo` are ordinary `Identifier`
tokens that produce `VarRef("%")` and `VarRef("%foo")` AST nodes.
No special token type is needed — pipeline references use the
same evaluation machinery as any other variable reference.

### Section Header Components

The `---` line is a section header that can carry three optional
components: a name, an output type annotation, and an input
contract pragma.

| Component | Purpose | Syntax | Example |
|-----------|---------|--------|---------|
| Name | Bind output for later sections | `%name` | `%validated` |
| Output type | Type annotation on output | `@Type` on name | `%validated@ValidatedConfig` |
| Input contract | What `%` must conform to | `expects: Type` | `expects: NginxConfig` |

```tinct
--- %validated@ValidatedConfig expects: RawData
--- expects: NginxConfig
--- %config@[host: String  port: Int]
--- %raw
```

**Output types** use the `@` annotation syntax, consistent with
tinct's existing `variable@Type` pattern. The annotation
attaches to the section name: `%name@Type` declares that this
section's output conforms to `Type`. Anonymous sections can also
carry output types: `%@Type`.

**Input contracts** use the `expects:` pragma keyword. This
declares the type that `%` (the previous section's output) must
conform to. The type checker validates the producing section's
output against the consuming section's `expects:` declaration.

Input contracts are primarily meaningful at file boundaries —
within a file, sections reference predecessors by name and the
type checker validates through normal inference. At file
boundaries, the consuming file cannot see the producer's
internals, so `expects:` serves as the interface declaration.

Pragma keys are predefined by the runtime, not user-extensible.
Other future pragmas (e.g., `format: json`) follow the same
key-value pattern on the `---` line.

### Multi-File Pipeline Chaining

`tinct eval` accepts multiple `.llt` files as pipeline stages
(see doc/whatif/templating.md §Multi-File Pipeline). Each file's
output becomes `%` for the next file:

```bash
tinct eval config.llt stdlib/out/yaml.llt
```

**Named sections are file-local.** The named-section map `Σ`
does not propagate across file boundaries. If `config.llt` ends
with `--- %result`, the name `%result` is local to that file.
The next file receives the output as `%` — the name is stripped
at the boundary.

A named final section is not an error — the name simply has no
consumers across files. The output is always bound as anonymous
`%` in the next file, enabling uniform chaining regardless of
the producing file's internal naming.

**Input contracts at file boundaries.** The consuming file
declares what it expects via `expects:` on its first `---`
header:

```tinct
# stdlib/out/yaml.llt
--- expects: [server: [host: String  port: Int]  workers: Int]
[emit [to-yaml %]]
```

The type checker validates `config.llt`'s output against
`yaml.llt`'s `expects:` declaration. Blame identifies the
boundary: "contract violation at file boundary: `config.llt`
output does not conform to `yaml.llt`'s input contract."

**Formal semantics** — multi-file pipeline:

```
eval_pipeline(files, ρ_base, input_thunk):
  θ₀ = input_thunk
  ∀i ∈ 1..n:
    θᵢ = eval_file(fileᵢ.documents, ρ_base, θᵢ₋₁)
    # Σᵢ is local to fileᵢ — not propagated
  ⇒ θₙ
```

Named sections (`Σ`) are scoped to the file. The only value
that crosses file boundaries is the anonymous output thunk `θ`.

### Combined Example

```tinct
# Current tinct
[
  double: [fn [n] [call $* $n 2]]
  nums: [call $range 1 11]
  result: [call $map $double $nums]
  sum: [call $reduce $+ 0 $result]
]
---
[call $str "Sum of doubles: " [call $str $$]]

# Proposed tinct
[
  double: [fn [n] [* n 2]]
  nums: [range 1 11]
  result: [map double nums]
  sum: [reduce + 0 result]
]
---
[str "Sum of doubles: " [str %]]
```

Multi-section pipeline with naming and contracts:

```tinct
--- %raw
[parse-csv input-file]

--- %cleaned@[name: String  validated: Bool]
[map [fn [row] [merge row [validated: true]]] %raw]

--- %summary
[
  total: [count %cleaned]
  valid: [count [filter [fn [r] r.validated] %cleaned]]
]

---
[
  data: %cleaned
  summary: %summary
  generated: [now]
]
```

Multi-file pipeline chaining:

```bash
tinct eval config.llt stdlib/out/yaml.llt
```

```tinct
# config.llt
[server: [host: "localhost"  port: 8080]  workers: 4]
```

```tinct
# stdlib/out/yaml.llt
--- expects: [server: [host: String  port: Int]  workers: Int]
[emit [to-yaml %]]
```

### Interaction with Lazy Evaluation

No impact. Thunks are created for expressions regardless of
whether references are spelled `$name` or `name`. The evaluator
resolves names via the environment chain in both cases. Implied
call produces the same `Call` AST node as explicit `call` — the
evaluator sees no difference.

Disambiguation happens at parse time via the head-position rule,
not at evaluation time. The parser has already decided whether a
`[]` is a call or data before any thunks are created. This
preserves Principle 4's guarantee that brackets can remain lazy —
the evaluator never needs to eagerly materialize the head of a
bracket expression to determine its role.

### Interaction with Type Inference

Implied call and bare-word references have no impact on the core
inference algorithm: `Call` nodes from implied call are structurally
identical to explicit `call` nodes, and variable references produce
the same env-lookup behavior regardless of spelling. Unification,
generalization, and substitution are unchanged.

However, the following type checker changes are required by this
proposal:

- **Pipeline binding rename**: `typecheck_document` currently
  inserts the section output type under key `"$"` for the next
  document's env. This must become `"%"`. Named-section bindings
  (`%name`) must also accumulate across the document loop.
- **Section output annotation**: `--- %name@Type` requires the
  type checker to validate the section's inferred output type
  against the declared annotation. This must be resolved against
  the post-body env (type aliases declared inside the section are
  visible). It is distinct from `TypeAssert` and does not support
  the `default:` fallback.
- **`expects:` input contract**: `--- expects: Type` requires
  checking that the current `%` binding's inferred type conforms to
  the declared type. Resolved against the pre-body env (incoming
  type). Emits a `TypeError` (advisory), consistent with the rest
  of the type system.
- **Cross-file pipeline**: Multi-file `tinct eval f1.llt f2.llt`
  requires `typecheck_file` to accept an `incoming_type: Option<Type>`
  representing `%`'s type from the preceding file, so that cross-file
  `expects:` contracts can be validated statically.

### Interaction with Row Polymorphism

Keys remain strings, so row polymorphism is unaffected. Record
types track string-keyed fields. The `...` row variable syntax is
unchanged.

### Interaction with String Interpolation

String interpolation (doc/whatif/string-interpolation.md) proposes
`i"Hello $name"`. With bare-word references removing `$` from
expressions, `$` inside interpolated strings becomes a string-
internal syntax — not part of the expression grammar:

```tinct
name: "Alice"
greeting: i"Hello $name, welcome"  # $ is interpolation marker
```

Outside strings, `name` is a reference (no `$` needed). Inside
`i"..."`, `$name` marks an interpolation point. The two contexts
don't conflict — `$` marks interpolation inside strings (like
Ruby's `#{}`) and disambiguation outside strings.

### Interaction with `$_` Desugaring

No impact. The `$_` implicit lambda desugaring operates on the
`VarRef("_")` AST node, not on the surface spelling. Under the new
syntax, bare `_` produces the same `Expr::VarRef("_")` AST node as
the current `$_`. The desugar pass sees no difference — both forms
trigger identical desugaring rules. Users can write bare `_` in all
positions where `$_` was previously written.

### Interaction with Macros

Macros (doc/whatif/macros.md) and functions share call syntax under
implied call: `[timed f x]` looks identical to a function call.
The macro expander, which runs after parsing, checks whether the
head is a registered macro and rewrites the AST. If not, the Call
node stands.

This is standard Lisp macro semantics: macros and functions are
invoked with the same syntax. No conflict with implied call.

### Interaction with the Formatter

The AST-based formatter introduced in `doc/whatif/parser-rewrite.md`
handles explicit/implied call preservation via the `implied: bool`
flag on `Call` AST nodes. No special tracking is needed — the
formatter reads the flag directly and emits `[call f x]` when
`implied` is false and `[f x]` when true. The author's choice is
preserved without inspecting the token stream.

**String rendering simplification.** Under the current tinct syntax,
the formatter must distinguish bare-word strings from quoted strings
(`hello` vs `"hello"`) — both parse to `Expr::Str` but must be
emitted differently. The AST-based formatter handles this via a
span-based source lookup on `ParseOutput.source` (see
`doc/whatif/parser-rewrite.md` §AST-Based Formatter, "String form
preservation"). Phase 2 of this proposal eliminates the problem
entirely: bare words in value position become `Expr::VarRef`
(variable references), never `Expr::Str`. All `Expr::Str` nodes
originate exclusively from quoted strings, so the formatter emits
`"..."` unconditionally and the span-peek logic is removed.

### Interaction with Structural Contracts

Structural contracts (doc/whatif/structural-contracts.md) declare
the expected shape of pipeline input via `$$@Type` and validate
constraints at runtime via `$validate`. With the unified syntax,
contracts split into two mechanisms: `expects:` for input
contracts and `@Type` for output types. Calls use bare words:

```tinct
# Current structural contract (structural-contracts.md syntax)
$$@NginxConfig

NginxConfig: [type [port: Int  hostname: String]]

nginx-schema: [
  port: [min: 1  max: 65535]
  hostname: [pattern: "^[a-z0-9.-]+$"]
]

[call $validate $nginx-schema $$]

---

[call $emit [call $to-nginx $$]]
```

```tinct
# With unified syntax — split contract mechanisms
--- expects: NginxConfig

NginxConfig: [type [port: Int  hostname: String]]

nginx-schema: [
  port: [min: 1  max: 65535]
  hostname: [pattern: "^[a-z0-9.-]+$"]
]

[validate nginx-schema %]

---

[emit [to-nginx %]]
```

The `expects:` pragma declares the input contract — this section
expects `%` to conform to `NginxConfig`. The `@Type` annotation
on the section name declares the output type — what this section
produces. The two mechanisms don't overlap: `expects:` is always
input, `@Type` is always output.

Named sections combine naturally with both contract forms:

```tinct
--- %raw
[parse-csv input-file]

--- %validated@ValidatedConfig expects: [name: String  port: Int  host: String]
[validate server-schema %]

--- %config@NginxConfig
[
  server: [host: %validated.host  port: %validated.port]
  tls: [enabled: true]
]

--- expects: NginxConfig
[emit [to-nginx %config]]
```

`--- %validated@ValidatedConfig expects: [...]` declares both:
this section expects its input (`%`, which is `%raw`'s output)
to have the given record type, and its output is bound as
`%validated` with type `ValidatedConfig`. Blame assignment
(structural-contracts.md §Blame Assignment) identifies the
boundary: "contract violation at `%raw` → `%validated`."

**Multi-file contracts.** At file boundaries, `expects:` on the
consuming file's first `---` header validates the producer's
output:

```tinct
# config.llt
[server: [host: "localhost"  port: 8080]  workers: 4]
```

```tinct
# stdlib/out/yaml.llt
--- expects: [server: [host: String  port: Int]  workers: Int]
[emit [to-yaml %]]
```

```bash
tinct eval config.llt stdlib/out/yaml.llt
```

The type checker validates `config.llt`'s output against
`yaml.llt`'s `expects:` declaration at the file boundary.

### Impact on Config Data

The primary cost: string-heavy configuration data requires quotes
on every string value:

```tinct
# Current — compact config
[server: [host: localhost  env: production  log-level: info]]

# Proposed — must quote strings
[server: [host: "localhost"  env: "production"  log-level: "info"]]
```

tinct remains terser than JSON (unquoted keys, no commas) but
loses the bare-string-value advantage. The tradeoff favors code
over config: functional code gets dramatically cleaner while config
data gets slightly more verbose.

### Impact on Code

Functional code becomes significantly cleaner:

```tinct
# Current
[call $collect
  [call $take 10
    [call $filter [fn [n] [call $= 0 [call $mod $n 2]]]
      [call $range 0]]]]

# Proposed
[collect
  [take 10
    [filter [fn [n] [= 0 [mod n 2]]]
      [range 0]]]]
```

The proposed syntax approaches Scheme in readability. Combined
with dict syntax for named fields, tinct occupies a unique point
in the design space: Lisp-ergonomic calls with first-class
structured data.

## What Would Change

### Lexer (src/lexer.rs)

**Current:** Classifies `$word` as `VarRef` and bare words as
`BareWord` (strings). `%` may already be accepted by
`is_bare_word_char` — verify before claiming Phase 1 adds it.
`Identifier` tokens are not yet access-context triggers.

**Proposed:**

- Bare words → `Identifier` tokens (references in value position)
- `$word` → `EscapedRef` tokens (reference with disambiguation
  marker, used in head and key positions)
- `%` confirmed as valid identifier character — `%word` is a
  regular `Identifier` token (no special token type)
- `Identifier` added to the set of access-context triggers, so
  `%name.field` and `name.field` produce dot-access chains (not
  separate tokens)
- Quoted strings remain `StringLiteral`
- Bare `$` (no following word) → syntax error (pipeline role
  moved to `%` by convention)

**Impact:** Major. `BareWord` is repurposed to `Identifier`,
`VarRef` is repurposed to `EscapedRef`, `Identifier` becomes an
access-context trigger alongside `EscapedRef` and `CloseBracket`.

### Parser (src/parser.rs)

**Current:** Checks first entry of `[]` for keywords (`call`,
`fn`, `type`). `BareWord` → string data. `VarRef` → reference
value.

**Proposed:** The bracket interpretation rules (§Implied Call) add
head-position analysis:

1. Keyword → special form (unchanged)
2. Keyed entry → dict (unchanged)
3. `Identifier` in head → `Call` node
4. `EscapedRef` in head → `Dict` node (data)
5. Literal in head → `Dict` node (data)

The parser also handles `--- %name@Type expects: Type` section
headers — name, output type annotation, and input contract
pragma.

**Impact:** Major. The parser gains head-position context
sensitivity and section header parsing.

### AST (src/parser.rs AST types)

**Current:** `Expr::String` for bare words, `Expr::VarRef` for
`$`-prefixed references.

**Proposed:** `Expr::VarRef` for all bare words in value position.
`Expr::String` only for quoted strings. `Call` nodes gain an
`implied: bool` field for formatter preservation.

**Impact:** Moderate.

### Evaluator (src/eval.rs)

**Current:** Resolves `VarRef` via environment lookup. Pipeline
binding uses `$` (from `$$`).

**Proposed:** Same resolution logic, applied to more `VarRef`
nodes. `eval_file_with_input()` changes:

- Pipeline binding name changes from `$` to `%`
- Named sections accumulate `%name` bindings in pipeline scope
- Multi-file pipeline: named sections (`Σ`) scoped per-file,
  only anonymous `%` crosses file boundaries

**Impact:** Minor. Core evaluation unchanged; pipeline binding
is a small extension to `eval_file_with_input()`.

### Type Checker (src/typecheck.rs)

**Current:** Infers `String` type for bare words, looks up type
environment for `VarRef` nodes.

**Proposed:** Looks up type environment for all bare words. String
literals (quoted) infer `String`. `Call` nodes from implied call
are identical to explicit `call` nodes. Validates `expects:`
input contracts and `@Type` output annotations on section
headers. At file boundaries, checks producer output against
consumer's `expects:` declaration.

**Impact:** Moderate.

### Formatter (src/formatter.rs)

**Current:** Token-stream formatter, to be replaced by the
AST-based formatter from `doc/whatif/parser-rewrite.md`.

**Proposed:** The AST-based formatter (prerequisite: parser-rewrite
Phase 3) gains these rendering rules: identifiers emitted as bare
words without `$` prefix; string literals emitted with `"` delimiters;
dict keys emitted unquoted; explicit/implied call form preserved via
the `Call.implied` AST flag.

**Impact:** Minor — the AST-based formatter already walks
`Annotated<File>`; new-syntax adds straightforward rendering rules
for the reformed token types on top of the parser-rewrite base.

### Error Messages (src/error.rs)

**Current:** References `$name` for variables, `[call $f ...]`
for calls.

**Proposed:** References `name` for variables, `[f ...]` for
calls. New suggestion: "Did you mean to quote this as a string?
Use `\"name\"`" for unresolved references that look like intended
string literals.

**Impact:** Moderate.

### All Existing tinct Files

**Current:** Every `.llt` file uses `$` for references, `call`
for application, `$$` for pipeline.

**Proposed:** Every `.llt` file must be migrated. Migration
tooling (Phase 2) automates this.

**Impact:** Fundamental. Every tinct file breaks.

## Phased Adoption

### Phase 1: `%` Pipeline Variable + Section Naming

Add `%` as a valid identifier character and section naming to
`---` lines. This phase is independent of bare-word references
and implied call — it works with current `$`-sigil syntax.

```tinct
--- %defaults
[host: localhost  port: 8080]

--- %overrides
[host: prod.example.com  tls: true]

---
[call $merge %defaults %overrides]
```

Implementation:

- Lexer: add `%` to valid identifier characters
- Parser: parse `--- %name` and `--- %name@Type` section headers,
  `expects:` pragma
- Evaluator: accumulate named bindings in pipeline scope
- Type checker: validate `expects:` contracts and `@Type` output
  annotations

This phase provides immediate value — named pipeline sections
are useful regardless of whether the other syntax changes are
adopted. No breaking changes.

### Phase 2: New Syntax Adoption

The reformed syntax replaces the current syntax. There is no user
code to migrate — all tinct files (stdlib, corpus, tests) are
internal and updated directly as part of this sprint.

The breaking change is that bare words in value position are now
variable references rather than strings. Existing files using
unquoted strings (`[host: localhost]`) will produce undefined-
variable errors unless the bare word is in scope. `$x` in value
position remains valid — both `x` and `$x` resolve as references.
`call` and `fn` remain valid keywords. `$$` is replaced by `%`.

### Prerequisites

- **Phase 1:** No dependencies. Pipeline naming is self-contained.
- **Phase 2:** Phase 1 stable. String interpolation design compatible
  with `$` as the interpolation marker inside `i"..."` — compatibility
  is guaranteed by construction (string-internal `$` is orthogonal to
  expression-grammar changes). The AST-based formatter (parser-rewrite
  Phase 3) is not required — the token-stream formatter handles the
  token rename cleanly: `Identifier(s)` renders as `s`, `EscapedRef(s)`
  renders as `$s`.

### Trigger

- Phase 1 (`%` pipeline) can be adopted immediately — no
  preconditions, no breaking changes.
- Phase 2 triggers when tinct shifts from primarily-configuration
  to primarily-programming use cases, tipping the data/code balance
  toward code.
- When LLM generation accuracy with `$` sigils and `call` keywords
  becomes a measurable problem.
- When user feedback identifies syntactic verbosity as a barrier to
  adoption.

## References

- doc/02-syntax.md §Variable References — "Bare words are always string
  literals. `$word` is always a variable reference." This proposal
  trades that uniformity for conventional syntax, using `$` as a
  rare disambiguator rather than a universal sigil.
- doc/01-introduction.md §Principle 3: Explicit Function Application — "Without
  `call`, the evaluator must eagerly materialize the head." This
  rationale is superseded by the parser-level head-position rule,
  which resolves ambiguity before evaluation.
- doc/01-introduction.md §Principle 2: One Bracket, One Structure — `[]` remains
  the only bracket type. Its interpretation depends on the head
  element, approaching Lisp semantics while preserving dict syntax.
- doc/09-documents.md §Document Structure, DOC-PIPELINE — Current `$$`
  pipeline semantics. This proposal extends it with `%`, named
  sections, and section pragmas.
- doc/whatif/string-interpolation.md — String interpolation
  proposal. Compatible: `$` becomes an interpolation marker inside
  `i"..."` strings, orthogonal to its disambiguator role outside
  strings.
- doc/whatif/call-aliases.md — Macro-based call forms. Compatible:
  macros and functions share implied call syntax, with the expander
  distinguishing them (standard Lisp semantics).
- doc/whatif/structural-contracts.md — Pipeline input contracts.
  Compatible: `$$@Type` splits into `expects:` pragma (input
  contracts) and `@Type` annotation (output types) on the `---`
  line. `$validate` becomes bare-word `validate`. Named sections
  enable per-boundary contract declarations.
- doc/whatif/templating.md — Multi-file pipeline and `$emit`.
  Compatible: file boundaries are always anonymous (`%`), named
  sections are file-local, consuming files declare input
  contracts via `expects:` on their first `---` header.
- Dolstra, E. (2006). "The Purely Functional Software Deployment
  Model." PhD thesis, Utrecht University. Ch. 4. — Nix's bare-
  identifier-as-reference model and attribute-name-as-string model,
  the closest precedent for tinct's proposed value/key distinction.
- McCarthy, J. (1960). "Recursive functions of symbolic expressions
  and their computation by machine, Part I." *Communications of the
  ACM*, 3(4), 184-195. — S-expression semantics: `(f x y)` is
  always application, `'(f x y)` is data. tinct's `[$f x y]`
  draws on this model: `$` is a call-suppression marker, not
  quote — it prevents call interpretation of the bracket form
  while still permitting evaluation of its contents. `[$f x y]`
  is closer to `(list f x y)` than to `'(f x y)`.
- Ford, B. (2004). "Parsing expression grammars: a recognition-
  based syntactic foundation." *POPL '04*, pp. 111-122. — PEGs
  support local syntactic checks (head-position rule) without
  environment coupling, confirming the disambiguation is
  parser-compatible.
