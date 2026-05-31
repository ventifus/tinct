# Quasiquoting

## Overview

Quasiquoting adds AST-as-data representation to tinct via `quote`, `unquote`,
and `unquote-splice`.

tinct is a data transformation language. Its core operation is transforming
structured data (dicts, lists) into other structured data. Quasiquoting extends
this to the language's own syntax — tinct code *is* structured data, and
quasiquoting makes it accessible as such.

```tinct
# Without quasiquoting — manual AST construction
ast-node: [type: "call"  fn: [type: "var"  name: "if"]
           args: [[type: "var"  name: "pred"]
                  [type: "var"  name: "body"]
                  [type: "literal"  value: []]]]

# With quasiquoting — natural syntax
ast-node: [quote [if pred body []]]
```

Quasiquoting provides:

- **Code-as-data capture** — `[quote expr]` produces a dict representing the
  AST of `expr` without evaluating it
- **Template-based AST construction** — build code using natural tinct syntax
  instead of manual dict construction
- **Computed splicing** — `[unquote expr]` and `[unquote-splice expr]` insert
  evaluated values into quoted templates
- **Macro ergonomics** — quasiquoting is the interface layer that makes
  procedural macros (`doc/whatif/macros.md`) practical to write
- **Runtime AST inspection** — programs can examine their own structure,
  enabling metaprogramming, debugging tools, and code generation

## Concepts

### Quote

`[quote expr]` captures `expr` as a data structure (dict) representing its AST,
without evaluating it:

```tinct
[quote [+ 1 2]]
# -> [type: "call"
#    fn: [type: "var"  name: "+"]
#    args: [[type: "literal"  value: 1]
#           [type: "literal"  value: 2]]]
```

### Unquote

`[unquote expr]` inside a `[quote ...]` evaluates `expr` and splices the result
into the quoted template:

```tinct
x: 42
[quote [+ [unquote x] 1]]
# -> [type: "call"
#    fn: [type: "var"  name: "+"]
#    args: [[type: "literal"  value: 42]   # x was evaluated and spliced
#           [type: "literal"  value: 1]]]
```

### Unquote-Splicing

`[unquote-splice expr]` splices a list of AST nodes into a quoted list position:

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

`quote`, `unquote`, and `unquote-splice` are keywords in the grammar. These are
special forms — `quote` fundamentally prevents evaluation of its argument, which
no regular function or builtin can do.

```tinct
[quote [if pred body []]]
[quote [+ [unquote computed-value] 1]]
```

### AST Dict Schema

The canonical schema is defined in `doc/whatif/ast-schema.md` — all consumers
(quasiquoting, macros, tinct-hosted formatter) share one definition to prevent
drift. Key conventions: `type:` string discriminator on every node, `[]` for
absent optionals, `span:` on every node, `schema-version: 1` on the root
`File` node.

A few representative examples:

```tinct
[quote [+ 1 2]]
# -> [type: "call"
#     fn:   [type: "var"     name: "+"]
#     args: [[type: "literal"  kind: "int"  value: 1  span: ...]
#            [type: "literal"  kind: "int"  value: 2  span: ...]]
#     implied: true
#     span: ...]

[quote config.host]
# -> [type: "dot-access"  target: [type: "var"  name: "config"]  field: "host"  span: ...]

[quote data.0]
# -> [type: "dot-access"  target: [type: "var"  name: "data"]  field: 0  span: ...]
```

See `doc/whatif/ast-schema.md` for the complete mapping of every `Expr` variant,
supporting types (`Entry`, `Param`, `Annotation`, `Document`), comment
embedding, and the `ast_to_dict_expr` / `ast_to_dict` / `dict_to_ast` Rust
function signatures.

### Why Special Forms

`quote` requires special syntax because it fundamentally prevents evaluation of
its argument. No regular function can do this — functions receive evaluated
arguments (or thunks in a lazy language, but even thunks are evaluated on
demand, not captured as AST). Dict constructors work as the low-level mechanism
but are too verbose for practical use. `quote` is syntactic sugar over dict
construction, but it is sugar that *cannot* be expressed as a function.

### Contextual Parsing

`unquote` and `unquote-splice` are only valid inside `quote`. The parser tracks
nesting depth: inside a `[quote ...]`, `[unquote ...]` switches back to
expression mode (evaluated). Nested quotes increment the depth; `unquote` only
evaluates at depth 1. This follows Lisp's established quasiquote semantics
(Bawden, 1999).

**`unquote-splice` position restriction:** `[unquote-splice expr]` is only valid
in a *list position* — inside the `args:` of a call or the `entries:` of a
dict, where the splice has a sequence to extend. It is a parse error at the
*top level* of a `[quote ...]` where there is no enclosing list to splice into.
Per Bawden (1999) Appendix A, `qq-expand` rejects `tag-comma-atsign?` at top
level: there is no meaningful semantics for splicing into a scalar position. The
parser enforces this: `[quote [unquote-splice xs]]` is an error;
`[quote [f [unquote-splice xs]]]` (splice into `f`'s arg list) is valid.

### Interaction with Lazy Evaluation

`[quote expr]` does *not* create a thunk — it converts the syntactic form of
`expr` into a dict when the `Expr::Quote` node is forced by the normal
evaluator. The resulting dict is an ordinary tinct value (a `Value::Dict`)
subject to normal lazy evaluation rules. `[unquote expr]` inside a quote
evaluates `expr` in the current runtime environment when the surrounding
`[quote]` is forced — the result is spliced into the dict structure. This is
runtime evaluation, not expansion-time evaluation: `[unquote]` fires at the same
point `[quote]` is forced, not during the `expand_macros` pass.

### Interaction with Type System

Quoted expressions have type `Dict` (specifically, a record type matching the
AST dict schema). `ast_to_dict` always produces a dict; `dict_to_ast` expects
one. The type checker does not need special rules for `quote` — it simply types
the result as `Dict`. In a future type system with more precise record types,
the schema could be typed more precisely (e.g.,
`[type: Str, fn: AstNode, args: Seq AstNode]` for a call node).

## Implementation

### Parser / Grammar

Three keywords are added: `quote`, `unquote`, `unquote-splice`. The parser
recognizes `[quote ...]` and produces `Expr::Quote` nodes. Inside `[quote ...]`,
`[unquote ...]` produces `Expr::Unquote` and `[unquote-splice ...]` produces
`Expr::UnquoteSplice`. Nesting depth is tracked to handle nested quotes
correctly. Contextual parsing (unquote only valid inside quote) adds a parsing
mode, similar to pattern-matching's pattern mode.

### AST

Three new `Expr` variants: `Quote(Box<Spanned<Expr>>)`,
`Unquote(Box<Spanned<Expr>>)`, `UnquoteSplice(Box<Spanned<Expr>>)`. A stable
`Expr -> Value::Dict` projection is defined. The AST dict schema becomes a
public API surface — any change to the `Expr` enum requires updating the schema,
and existing macros may break.

### Evaluator

`Expr::Quote` converts its argument to a dict representation via `ast_to_dict()`.
`Expr::Unquote` inside a quote evaluates its argument and splices the result.
`Expr::UnquoteSplice` evaluates to a sequence and splices each element into the
enclosing list position.

`ast_to_dict` and `dict_to_ast` are Rust functions that handle every `Expr`
variant exhaustively.

### Type Checker

`[quote expr]` has type `Dict`. `[unquote expr]` inside a quote has type `Dict`
(it must produce an AST dict node). No special inference — quote is transparent
to the type system. One new inference rule: `Quote -> Dict`.

### Builtins

`eval-ast` builtin: takes a dict (AST representation), converts to `Expr` via
`dict_to_ast`, and evaluates. This closes the code-as-data loop — `quote`
converts code to data, `eval-ast` converts data back to code and runs it.

`eval-ast` is a powerful primitive that enables runtime code generation. Security
implications: `eval-ast` can execute arbitrary code, which interacts with
sandboxing (doc/12-tooling.md Sandboxing).

## References

- McCarthy, J. (1960). "Recursive functions of symbolic expressions and their computation by machine, Part I." *Communications of the ACM*, 3(4), 184–195. — Original `quote` in LISP. Establishes code-as-data as a language primitive.
- Bawden, A. (1999). "Quasiquotation in Lisp." In *PEPM '99*, pp. 4–12. ACM. — The definitive formal treatment of quasiquotation semantics. Defines the nesting depth algebra: `quote` increments depth, `unquote` decrements it, evaluation occurs only at depth 0. Bawden gives a recursive expansion function that provably handles arbitrarily nested quote/unquote combinations and proves the algebra is a left inverse of quotation (unquoting a quoted value recovers the original). Directly applicable to tinct's contextual parsing design (nesting depth tracker).
- Elixir documentation: Quote and unquote. `quote do ... end` and `unquote(expr)` — closest practical precedent for tinct's model. Elixir represents AST as 3-tuples; tinct uses dicts.
- Flatt, M. (2002). "Composable and compilable macros: you want it when?" In *ICFP '02*, pp. 72–83. ACM. — Phase separation for compile-time evaluation. In tinct, `[unquote]` evaluates at runtime (when `[quote]` is forced), not at expansion time — but Flatt's phase model is relevant to how `[macro]` bodies execute in a separate compile-time context (see `doc/whatif/macros.md`).
- Taha, W. & Sheard, T. (2000). "MetaML and multi-stage programming with explicit annotations." *Theoretical Computer Science*, 248(1–2), 211–242. — Multi-stage programming with typed code quotation. Provides a type-theoretic foundation for quote/unquote where quoted code has type `Code a` rather than untyped `Dict`. Relevant if tinct pursues typed quotation in the future.
