# What If: Quasiquoting for tinct

What would it take to add quasiquoting --- AST-as-data representation with
`quote`/`unquote` --- to tinct?

## Current State

tinct has no mechanism for treating code as data. Expressions are
evaluated, not quoted. There is no way to:

- Capture an expression as a data structure without evaluating it
- Construct AST nodes programmatically using tinct's own dict syntax
- Splice computed values into code templates

The `doc/whatif/macros.md` proposal identifies quasiquoting as a
prerequisite for procedural AST macros. Macros receive AST-as-data and
return AST-as-data --- quasiquoting is the mechanism that converts between
code and data.

### Why Quasiquoting Matters for tinct

tinct is a data transformation language. Its core operation is
transforming structured data (dicts, lists) into other structured data.
Quasiquoting extends this to the language's own syntax --- tinct code
*is* structured data, and quasiquoting makes it accessible as such.

```tinct
# Without quasiquoting --- manual AST construction
ast-node: [type: "call"  fn: [type: "var"  name: "if"]
           args: [[type: "var"  name: "pred"]
                  [type: "var"  name: "body"]
                  [type: "literal"  value: []]]]

# With quasiquoting --- natural syntax
ast-node: [quote [if pred body []]]
```

### What's Missing

1. **Code-as-data** --- no way to capture an expression as a data
   structure without evaluating it
2. **Template construction** --- programmatic AST construction requires
   manual dict building, which is verbose and error-prone
3. **Computed splicing** --- no mechanism to insert computed values into
   code templates
4. **AST dict schema** --- no defined mapping between `Expr` variants
   and tinct dict representations

## What Quasiquoting Would Provide

- **Code-as-data capture** --- `[quote expr]` produces a dict
  representing the AST of `expr` without evaluating it
- **Template-based AST construction** --- build code using natural tinct
  syntax instead of manual dict construction
- **Computed splicing** --- `[unquote expr]` and `[unquote-splice expr]`
  insert evaluated values into quoted templates
- **Macro ergonomics** --- quasiquoting is the interface layer that makes
  procedural macros (`doc/whatif/macros.md`) practical to write
- **Runtime AST inspection** --- programs can examine their own structure,
  enabling metaprogramming, debugging tools, and code generation

## Concepts

### Quote

`[quote expr]` captures `expr` as a data structure (dict) representing
its AST, without evaluating it:

```tinct
[quote [+ 1 2]]
# -> [type: "call"
#    fn: [type: "var"  name: "+"]
#    args: [[type: "literal"  value: 1]
#           [type: "literal"  value: 2]]]
```

### Unquote

`[unquote expr]` inside a `[quote ...]` evaluates `expr` and splices
the result into the quoted template:

```tinct
x: 42
[quote [+ [unquote x] 1]]
# -> [type: "call"
#    fn: [type: "var"  name: "+"]
#    args: [[type: "literal"  value: 42]   # x was evaluated and spliced
#           [type: "literal"  value: 1]]]
```

### Unquote-Splicing

`[unquote-splice expr]` splices a list of AST nodes into a quoted
list position:

```tinct
extra-args: [[type: "literal"  value: 3] [type: "literal"  value: 4]]
[quote [+ 1 2 [unquote-splice extra-args]]]
# -> [type: "call"
#    fn: [type: "var"  name: "+"]
#    args: [[type: "literal"  value: 1]
#           [type: "literal"  value: 2]
#           [type: "literal"  value: 3]
#           [type: "literal"  value: 4]]]
```

## Design

Add `quote`, `unquote`, and `unquote-splice` as keywords in the grammar.
These are special forms --- `quote` fundamentally prevents evaluation of
its argument, which no regular function or builtin can do.

```tinct
[quote [if pred body []]]
[quote [+ [unquote computed-value] 1]]
```

### AST Dict Schema

Each AST node type maps to a tinct dict with a `type` discriminator key:

| AST Node | Dict Representation |
|----------|-------------------|
| `42` | `[type: "literal"  kind: "int"  value: 42]` |
| `"hello"` | `[type: "literal"  kind: "str"  value: "hello"]` |
| `true` | `[type: "literal"  kind: "bool"  value: true]` |
| `x` | `[type: "var"  name: "x"]` |
| `x.field` | `[type: "dot-access"  target: [type: "var"  name: "x"]  field: "field"]` |
| `[a b c]` | `[type: "dict"  entries: [...]]` |
| `[f x]` | `[type: "call"  fn: [type: "var"  name: "f"]  args: [...]]` |
| `[fn [x] body]` | `[type: "fn"  params: [...]  body: ...]` |

The schema should:

- **Use string `type` discriminator** --- `[type: call ...]`,
  `[type: var ...]`, etc. This is the tagged-union convention already
  used by `try` results (`[ok: ...]` / `[err: ...]`).
- **Mirror the `Expr` enum** --- one dict shape per `Expr` variant, with
  fields matching the Rust struct fields.
- **Include spans** --- `[span: [start: [line: 1 col: 5] end: [line: 1 col: 12]]]`
  on every node. Macro-generated nodes carry the expansion site's span.
- **Be versionable** --- add a `version` field to the root if schema
  changes are needed later.

### Why Special Forms

`quote` requires special syntax because it fundamentally prevents
evaluation of its argument. No regular function can do this --- functions
receive evaluated arguments (or thunks in a lazy language, but even thunks
are evaluated on demand, not captured as AST). Dict constructors work as
the low-level mechanism but are too verbose for practical use. `quote` is
syntactic sugar over dict construction, but it is sugar that *cannot* be
expressed as a function.

### Contextual Parsing

`unquote` and `unquote-splice` are only valid inside `quote`. The parser
must track nesting depth: inside a `[quote ...]`, `[unquote ...]` switches
back to expression mode (evaluated). Nested quotes increment the depth;
`unquote` only evaluates at depth 1. This follows Lisp's established
quasiquote semantics (Bawden, 1999).

### Interaction with Lazy Evaluation

`[quote expr]` does *not* create a thunk --- it converts the syntactic
form of `expr` into a dict at parse/expansion time. The resulting dict is
an ordinary tinct value (a `Value::Dict`) subject to normal lazy
evaluation rules. `[unquote expr]` inside a quote evaluates `expr` eagerly
during quote processing --- the result is spliced into the dict structure.
This is expansion-time evaluation, not runtime evaluation.

### Interaction with Type System

Quoted expressions have type `Dict` (specifically, a record type matching
the AST dict schema). `ast_to_dict` always produces a dict; `dict_to_ast`
expects one. The type checker does not need special rules for `quote` ---
it simply types the result as `Dict`. In a future type system with more
precise record types, the schema could be typed more precisely (e.g.,
`[type: Str, fn: AstNode, args: Seq AstNode]` for a call node).

## What Would Change

### Parser / Grammar

**Current:** Keywords `call`, `fn`, `type` are recognized as special forms.
No quoting mechanism exists.

**Proposed:** Add three keywords: `quote`, `unquote`, `unquote-splice`.
The parser recognizes `[quote ...]` and produces `Expr::Quote` nodes.
Inside `[quote ...]`, `[unquote ...]` produces `Expr::Unquote` and
`[unquote-splice ...]` produces `Expr::UnquoteSplice`. Nesting depth
is tracked to handle nested quotes correctly.

**Impact:** Moderate. Three new keywords in the denylist. Contextual
parsing (unquote only valid inside quote) adds a parsing mode, similar
to pattern-matching's pattern mode.

### AST

**Current:** `Expr` enum represents all tinct expressions. No
code-as-data mechanism.

**Proposed:** Add three new `Expr` variants: `Quote(Box<Spanned<Expr>>)`,
`Unquote(Box<Spanned<Expr>>)`, `UnquoteSplice(Box<Spanned<Expr>>)`.
Define a stable `Expr -> Value::Dict` projection.

**Impact:** Major. Three new AST variants. The AST dict schema becomes a
public API surface --- any change to the `Expr` enum requires updating
the schema, and existing macros may break.

### Evaluator

**Current:** All expressions are evaluated to values. No mechanism to
capture expressions as data.

**Proposed:** `Expr::Quote` converts its argument to a dict representation
via `ast_to_dict()`. `Expr::Unquote` inside a quote evaluates its
argument and splices the result. `Expr::UnquoteSplice` evaluates to a
sequence and splices each element into the enclosing list position.

**Impact:** Moderate. New evaluation rules for three `Expr` variants.
`ast_to_dict` and `dict_to_ast` are new Rust functions that must handle
every `Expr` variant exhaustively.

### Type Checker

**Current:** No quote-related type rules.

**Proposed:** `[quote expr]` has type `Dict`. `[unquote expr]` inside a
quote has type `Dict` (it must produce an AST dict node). No special
inference --- quote is transparent to the type system.

**Impact:** Minor. One new inference rule: `Quote -> Dict`.

### Builtins

**Current:** No AST manipulation builtins.

**Proposed:** Add `eval-ast` builtin: takes a dict (AST representation),
converts to `Expr` via `dict_to_ast`, and evaluates. This closes the
code-as-data loop --- `quote` converts code to data, `eval-ast` converts
data back to code and runs it.

**Impact:** Moderate. `eval-ast` is a powerful primitive that enables
runtime code generation. Security implications: `eval-ast` can execute
arbitrary code, which interacts with sandboxing (doc/12-tooling.md Sandboxing).

## Phased Adoption

### Phase 1: AST Dict Schema

Define the dict representation for every `Expr` variant. Document in
doc/15-ast.md. No syntax changes --- this is a specification.

Implement `ast_to_dict(expr: &Expr) -> Value` and
`dict_to_ast(value: &Value) -> Result<Expr, Error>` in Rust. These are
the serialization/deserialization functions that `quote`/`unquote` use
internally.

### Phase 2: `quote` Special Form

Add `quote` keyword to the grammar. `[quote expr]` produces an
`Expr::Quote` AST node. The evaluator converts the quoted expression
to its dict representation (calling `ast_to_dict`).

No `unquote` yet --- Phase 2 quotes are opaque. Useful for:
- Inspecting AST structure at runtime
- Testing the AST dict schema
- Building toward macros incrementally

### Phase 3: `unquote` and `unquote-splice`

Add `unquote` and `unquote-splice` keywords. Valid only inside `quote`.
The evaluator walks the quoted AST, evaluating `unquote` expressions
and splicing results into the dict.

This phase completes the quasiquoting system. Combined with
`dict_to_ast` as a `eval-ast` builtin, users can construct and
execute code programmatically.

### Phase 4: Macro Integration

Connect quasiquoting to the macro system (`doc/whatif/macros.md`):
- `[defmacro name [params] body]` registers a compile-time function
- Macro bodies use `quote`/`unquote` to construct return ASTs
- Expansion pipeline: `parse -> expand_macros -> typecheck -> eval`

### Prerequisites

- **Phase 1:** Stable `Expr` enum --- changes to AST node types break the
  schema. The grammar should be stable before committing to an AST dict
  format.
- **Phase 2:** Phase 1 complete.
- **Phase 3:** Phase 2 complete. Pattern matching
  (`doc/whatif/pattern-matching.md`) Phase 3 (dict destructuring) makes
  working with quoted AST much more ergonomic.
- **Phase 4:** Macros (`doc/whatif/macros.md`) Phase 2 complete.

### Trigger

- The macro system (`doc/whatif/macros.md`) is approved for
  implementation --- quasiquoting is a prerequisite
- A second syntactic desugaring is needed beyond `_` (e.g., string
  interpolation, pattern matching desugar) --- confirms the need for
  user-extensible syntax
- Users need runtime AST inspection or code generation

## References

- McCarthy, J. (1960). "Recursive functions of symbolic expressions
  and their computation by machine, Part I." *Communications of the
  ACM*, 3(4), 184--195. --- Original `quote` in LISP. Establishes
  code-as-data as a language primitive.
- Bawden, A. (1999). "Quasiquotation in Lisp." In *PEPM '99*, pp.
  4--12. ACM. --- Formal treatment of quasiquotation semantics.
  Defines nesting depth rules for nested quotes and the interaction
  between quote and unquote levels. Directly applicable to tinct's
  contextual parsing design.
- Elixir documentation: Quote and unquote. `quote do ... end` and
  `unquote(expr)` --- closest practical precedent for tinct's model.
  Elixir represents AST as 3-tuples; tinct uses dicts.
- Flatt, M. (2002). "Composable and compilable macros: you want it
  when?" In *ICFP '02*, pp. 72--83. ACM. --- Phase separation for
  compile-time evaluation. Relevant to the expansion-time evaluation
  semantics of `unquote`.
- Taha, W. & Sheard, T. (2000). "MetaML and multi-stage programming
  with explicit annotations." *Theoretical Computer Science*, 248(1--2),
  211--242. --- Multi-stage programming with typed code quotation.
  Provides a type-theoretic foundation for quote/unquote where quoted
  code has type `Code a` rather than untyped `Dict`. Relevant if tinct
  pursues typed quotation in the future.
