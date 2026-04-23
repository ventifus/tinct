# What If: Quasiquoting for tinct

What would it take to add quasiquoting — AST-as-data representation with
`quote`/`unquote` — to tinct?

## Current State

tinct has no mechanism for treating code as data. Expressions are
evaluated, not quoted. There is no way to:

- Capture an expression as a data structure without evaluating it
- Construct AST nodes programmatically using tinct's own dict syntax
- Splice computed values into code templates

The `doc/whatif/macros.md` proposal identifies quasiquoting as a
prerequisite for procedural AST macros (Approach B). Macros receive
AST-as-data and return AST-as-data — quasiquoting is the mechanism
that converts between code and data.

### Why Quasiquoting Matters for tinct

tinct is a data transformation language. Its core operation is
transforming structured data (dicts, lists) into other structured data.
Quasiquoting extends this to the language's own syntax — tinct code
*is* structured data, and quasiquoting makes it accessible as such.

```lisp
# Without quasiquoting — manual AST construction
ast-node: [type: call  fn: [type: var  name: if]
           args: [[type: var  name: pred]
                  [type: var  name: body]
                  [type: literal  value: []]]]

# With quasiquoting — natural syntax
ast-node: [quote [call $if $pred $body []]]
```

## Concepts

### Quote

`[quote expr]` captures `expr` as a data structure (dict) representing
its AST, without evaluating it:

```lisp
[quote [call $+ 1 2]]
# → [type: call
#    fn: [type: var  name: +]
#    args: [[type: literal  value: 1]
#           [type: literal  value: 2]]]
```

### Unquote

`[unquote expr]` inside a `[quote ...]` evaluates `expr` and splices
the result into the quoted template:

```lisp
x: 42
[quote [call $+ [unquote $x] 1]]
# → [type: call
#    fn: [type: var  name: +]
#    args: [[type: literal  value: 42]   # $x was evaluated and spliced
#           [type: literal  value: 1]]]
```

### Unquote-Splicing

`[unquote-splice expr]` splices a list of AST nodes into a quoted
list position:

```lisp
extra-args: [[type: literal  value: 3] [type: literal  value: 4]]
[quote [call $+ 1 2 [unquote-splice $extra-args]]]
# → [type: call
#    fn: [type: var  name: +]
#    args: [[type: literal  value: 1]
#           [type: literal  value: 2]
#           [type: literal  value: 3]
#           [type: literal  value: 4]]]
```

## Approaches

### Approach A: Special Forms (`quote`/`unquote` Keywords)

Add `quote`, `unquote`, and `unquote-splice` as keywords in the grammar:

```lisp
[quote [call $if $pred $body []]]
[quote [call $+ [unquote $computed-value] 1]]
```

**AST representation:** Each AST node type maps to a tinct dict with a
`type` discriminator key:

| AST Node | Dict Representation |
|----------|-------------------|
| `42` | `[type: literal  kind: int  value: 42]` |
| `hello` | `[type: literal  kind: str  value: hello]` |
| `true` | `[type: literal  kind: bool  value: true]` |
| `$x` | `[type: var  name: x]` |
| `$x.field` | `[type: dot-access  target: [type: var  name: x]  field: field]` |
| `[a b c]` | `[type: dict  entries: [...]]` |
| `[call $f $x]` | `[type: call  fn: [type: var  name: f]  args: [...]]` |
| `[fn [x] body]` | `[type: fn  params: [...]  body: ...]` |

**Pros:**
- Natural syntax using tinct's bracket notation
- `quote`/`unquote` is a well-understood model (Lisp, Elixir, Racket)
- Three keywords added to grammar (`quote`, `unquote`, `unquote-splice`)
- AST-as-dict fits tinct's data model — manipulate code with `$map`,
  `$filter`, `$get`

**Cons:**
- Three new keywords in the denylist
- AST dict schema must be defined and stabilized
- Changes to `Expr` enum break the schema (versioning needed?)
- `unquote` only valid inside `quote` — contextual parsing

## Recommendation

**Approach A: `quote`/`unquote`/`unquote-splice` as special form
keywords, with phased adoption tied to the macro system.**

### Rationale

1. **Quote requires special syntax.** Approaches B (builtins) and C
   (strings) don't work — quote fundamentally prevents evaluation of
   its argument, which no regular function can do.

2. **Dict constructors are the escape hatch, not the interface.**
   Approach D works as the low-level mechanism but is too verbose for
   practical use. `quote` is syntactic sugar over dict construction.

3. **AST-as-dict fits tinct perfectly.** tinct's core operation is
   transforming structured data. Making the language's own AST accessible
   as dicts enables tinct to transform its own code — this is the
   "one language" philosophy applied recursively.

4. **Prerequisite for procedural macros.** `doc/whatif/macros.md`
   recommends procedural AST macros (Approach B) where macro functions
   receive and return AST-as-data. Quasiquoting is the ergonomic layer
   that makes procedural macros usable.

### AST Dict Schema

The AST-as-dict representation must be defined carefully because
changes to it break existing macros. The schema should:

- **Use string `type` discriminator** — `[type: call ...]`,
  `[type: var ...]`, etc. This is the tagged-union convention already
  used by `$try` results (`[ok: ...]` / `[err: ...]`).
- **Mirror the `Expr` enum** — one dict shape per `Expr` variant, with
  fields matching the Rust struct fields.
- **Include spans** — `[span: [start: [line: 1 col: 5] end: [line: 1 col: 12]]]`
  on every node. Macro-generated nodes carry the expansion site's span.
- **Be versionable** — add a `version` field to the root if schema
  changes are needed later.

### Phased Adoption

#### Phase 1: AST Dict Schema

Define the dict representation for every `Expr` variant. Document in
DESIGN.md. No syntax changes — this is a specification.

Implement `ast_to_dict(expr: &Expr) -> Value` and
`dict_to_ast(value: &Value) -> Result<Expr, Error>` in Rust. These are
the serialization/deserialization functions that `quote`/`unquote` use
internally.

#### Phase 2: `quote` Special Form

Add `quote` keyword to the grammar. `[quote expr]` produces an
`Expr::Quote` AST node. The evaluator converts the quoted expression
to its dict representation (calling `ast_to_dict`).

No `unquote` yet — Phase 2 quotes are opaque. Useful for:
- Inspecting AST structure at runtime
- Testing the AST dict schema
- Building toward macros incrementally

#### Phase 3: `unquote` and `unquote-splice`

Add `unquote` and `unquote-splice` keywords. Valid only inside `quote`.
The evaluator walks the quoted AST, evaluating `unquote` expressions
and splicing results into the dict.

This phase completes the quasiquoting system. Combined with
`dict_to_ast` as a `$eval-ast` builtin, users can construct and
execute code programmatically.

#### Phase 4: Macro Integration

Connect quasiquoting to the macro system (`doc/whatif/macros.md`):
- `[defmacro name [params] body]` registers a compile-time function
- Macro bodies use `quote`/`unquote` to construct return ASTs
- Expansion pipeline: `parse → expand_macros → typecheck → eval`

### Prerequisites

- Pattern matching (`doc/whatif/pattern-matching.md`) — macro functions
  need to pattern match on AST dict structure. Expressible with
  `$type-of` + `$if` chains, but much better with `[match]`.
- Stable `Expr` enum — changes to AST node types break the schema.
  The grammar should be stable before committing to an AST dict format.

### Trigger

Adopt when:
- The macro system (`doc/whatif/macros.md`) is approved for
  implementation — quasiquoting is a prerequisite
- A second syntactic desugaring is needed beyond `$_` (e.g., string
  interpolation, pattern matching desugar) — confirms the need for
  user-extensible syntax
- Users need runtime AST inspection or code generation

## References

- McCarthy, J. (1960). "Recursive functions of symbolic expressions
  and their computation by machine, Part I." *Communications of the
  ACM*, 3(4), 184–195. — Original `quote` in LISP.
- Bawden, A. (1999). "Quasiquotation in Lisp." In *PEPM '99*, pp.
  4–12. ACM. — Formal treatment of quasiquotation semantics.
- Elixir documentation: Quote and unquote. `quote do ... end` and
  `unquote(expr)` — closest practical precedent for tinct's model.
- Flatt, M. (2002). "Composable and compilable macros: you want it
  when?" In *ICFP '02*, pp. 72–83. ACM. — Phase separation for
  compile-time evaluation.
