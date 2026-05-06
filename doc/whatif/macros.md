# What If: Desugaring as Macros

What would it take to unify tinct's syntactic sugar under a macro
system?

## Current State

tinct has multiple syntactic sugar mechanisms implemented as separate, disconnected systems:

1. **`_` implicit lambda** --- hardcoded in `eval.rs:66-71` (being moved to `src/desugar.rs` as a pre-typecheck AST pass)
2. **Stdlib sugar** --- `->`, `when`, `unless`, `cond`, `>=`, `<=`, `>`, `compose` --- ordinary lazy functions in `stdlib/prelude.llt`
3. **Parser-level sugar** --- access chains (`data.name` -> `DotAccess`), keyword forms (`call`, `fn`, `type`)
4. **Runtime scoping** --- document pipeline (`---`/`%`), dict letrec

Each new piece of sugar requires a different implementation strategy: Rust AST rewrite for `_`, stdlib function for `when`, grammar rule for access chains. There is no unified mechanism for defining syntactic transformations, and users cannot define their own.

### What Is Actually Desugaring?

Not everything called "sugar" is the same kind of transformation. A useful taxonomy:

| Category | Examples | Level | User-extensible? |
|----------|----------|-------|-------------------|
| **Syntactic desugaring** | `_` -> `[fn [_] ...]` | AST -> AST | No (Rust code) |
| **Functional sugar** | `->`, `when`, `>=` | Function composition | Yes (stdlib) |
| **Lexical sugar** | `data.name` -> `DotAccess(...)` | Tokenizer/parser | No (grammar rule) |
| **Semantic scoping** | `---`/`%`, dict letrec | Evaluator | No (core semantics) |

Only the first category --- syntactic desugaring --- is a candidate for macros. Functional sugar already works fine as lazy functions. Lexical sugar and semantic scoping are below the level macros operate.

Today, `_` is the only syntactic desugaring. String interpolation (doc/02-syntax.md, "if added") would be the second. The question is whether a general macro system is worth building for these, or whether hardcoded AST passes are sufficient.

### What's Missing

1. **User-defined syntactic transformations** --- users cannot define new
   binding forms, control flow constructs, or structural sugar
2. **Unified desugaring mechanism** --- each new piece of sugar requires a
   different implementation strategy (Rust code, stdlib function, grammar
   rule)
3. **AST-as-data access** --- no way to inspect or manipulate tinct code
   as tinct data structures
4. **Compile-time computation** --- all computation defers to runtime; no
   mechanism for expansion-time evaluation

## What Macros Would Provide

- **User-extensible syntax** --- domain-specific binding forms, control flow,
  and structural transformations defined in tinct, not Rust
- **Unified desugaring** --- all syntactic sugar (`_`, string interpolation,
  future constructs) expressed as macro rules rather than hardcoded passes
- **"One language" philosophy** --- sugar is defined in tinct using the same
  data transformation tools (`map`, `filter`, `get`) users already know
- **Zero-cost abstraction** --- macros that expand to inline code avoid
  thunk creation, eliminating per-call overhead for strict operations
- **Self-hosting path** --- reduces the Rust surface area by expressing
  syntactic transformations in tinct itself

## Interaction with Lazy Evaluation

Laziness and macros serve overlapping but distinct purposes.

### Why Laziness Reduces Macro Need

In strict languages, macros are essential for:
- **Short-circuit evaluation**: `and(a, b)` evaluates both args; you need a macro for `(and a b)` that skips `b` when `a` is false.
- **Conditional execution**: `if-then-else` must be a special form or macro, not a function.
- **Deferred computation**: Avoiding work that might not be needed.

tinct is lazy. All of these work as ordinary functions:
```tinct
# These are functions in stdlib/prelude.llt, not macros
when:   [fn [pred body] [if pred body []]]
unless: [fn [pred body] [if pred [] body]]
and:    [fn [a b] [if a b false]]
or:     [fn [a b] [if a true b]]
```

Because tinct uses call-by-need evaluation, `[when false [expensive]]` never forces `expensive`. Laziness gives you the main benefit of macros --- deferred evaluation --- for free.

Nix and Jsonnet demonstrate this at scale: neither has macros, and laziness covers most use cases. Nix's module system (`lib.mkIf`, `lib.mkOverride`) is built entirely from lazy functions returning tagged attribute sets.

### Where Laziness Falls Short

Macros provide things lazy functions cannot:

1. **New syntax** --- Functions cannot change how code is parsed. `_` creates a syntax that doesn't look like a function call.
2. **Compile-time computation** --- Functions defer to runtime. Macros can compute at expansion time, eliminating overhead.
3. **Structural transformation** --- Functions receive values. Macros receive AST and can restructure it (reorder, duplicate, elide subexpressions based on syntactic shape).
4. **Binding introduction** --- Functions cannot introduce new variable bindings in the caller's scope. Macros can expand to forms that bind names.
5. **Zero-cost abstraction** --- Every function call in tinct creates a thunk. A macro that expands to inline code avoids thunk creation.

The overlap means many stdlib functions (`when`, `>=`, `compose`) would NOT benefit from becoming macros --- they work fine as lazy functions. Macros are for the cases laziness can't cover: binding, structural transformation, and new syntax.

## `_` as the Acid Test

The acid test for the macro system: can `_` desugaring be expressed as a user-definable macro rather than a hardcoded Rust pass?

The `_` transformation requires (per doc/04-functions.md `_` Desugaring):

1. **DIRECT predicate** --- identify `VarRef("_")` or access chains rooted at `_` (e.g., `_.name`, `_[0]`)
2. **Top-down WRAP check** --- examine raw children of Call, Dict, DotAccess, BracketAccess, RangeAccess before recursing
3. **Func-position exclusion** --- `_` in function position of a Call does NOT trigger wrapping
4. **Depth-based shadowing** --- inside `[fn [_] ...]`, the `_` parameter shadows `_` desugaring
5. **Lambda wrapping** --- wrap the containing expression in `[fn [_] expr]` with span preservation

As a procedural macro operating on AST-dicts:

```tinct
# DIRECT predicate: is this node _ or an access chain rooted at _?
# Note: bracket-access removed by accepted access-pipeline whatif;
# pipe (lhs | rhs) replaces chained dynamic access — check lhs for _.
direct?: [fn [node]
  [or
    [and [= node.type "var"]        [= node.name "_"]]
    [and [= node.type "dot-access"] [direct? node.target]]
    [and [= node.type "pipe"]       [direct? node.lhs]]]]

# Check if any child of a node is DIRECT
has-direct-child?: [fn [node]
  [cond [
    [[= node.type "call"]
      [any? direct? node.args]]
    [[= node.type "dict"]
      [any? [fn [entry] [direct? entry.value]] node.entries]]
    [true
      [direct? node]]
  ]]]

# The macro: wrap expression in [fn [_] expr] if it has a DIRECT child
[defmacro desugar-underscore [expr]
  [if [has-direct-child? expr]
    [quote [fn [_] [unquote expr]]]
    expr]]
```

This demonstrates that a procedural macro system with AST-as-dict is powerful enough to express `_`. The DIRECT predicate, child inspection, and conditional wrapping all use ordinary tinct functions (`any?`, `=`, `cond`) applied to AST structure.

Depth-based shadowing would be handled by the macro expander itself: when expanding inside a `[fn [_] ...]` body, `_` is bound and the macro does not fire --- the same scoping rules that apply to any hygienic macro.

## Design

Macros are tinct functions that receive AST-as-data and return
AST-as-data. This is the "code is data" approach: tinct values (dicts,
lists, strings) represent AST nodes, and macro expansion is just data
transformation --- the same thing tinct already does.

### Syntax

```tinct
# AST is represented as tinct dicts
# [f x y] is the dict:
#   [type: "call"  fn: [type: "var"  name: "f"]  args: [[type: "var"  name: "x"] [type: "var"  name: "y"]]]

# A macro is a function from AST-dict to AST-dict
[defmacro when [pred-ast body-ast]
  [type: "call"
   fn: [type: "var"  name: "if"]
   args: [pred-ast  body-ast  [type: "literal"  value: []]]]]

# Or with quote/unquote syntax sugar:
[defmacro when [pred body]
  [quote [if [unquote pred] [unquote body] []]]]
```

### Expansion Pipeline

```
source -> parse -> quote_macros -> expand (call macro fns on quoted AST) -> typecheck -> eval
```

- `[defmacro name [params] body]` registers a compile-time function
- When `[name arg1 arg2 ...]` appears in source, the parser quotes the arguments (converts AST to tinct dicts) and calls the macro function with the quoted forms
- The macro function returns a tinct dict representing the expanded AST
- The expander converts the dict back to AST and continues expansion
- `quote` converts code to its AST-dict representation; `unquote` splices values into quoted code

### Hygiene Model

- **Not automatic** --- macro authors must manage variable names
- `[gensym]` builtin provides fresh unique names for introduced bindings
- Convention over enforcement: macros should use `gensym` for internal bindings
- This matches Template Haskell and early Common Lisp --- hygiene is opt-in

`gensym` names use a prefix containing `:` (a character forbidden in bare
words), making collision structurally impossible: a user cannot write
`:gensym:0` as a bare-word identifier in source. Names have the form
`:gensym:N` where N is a monotonically increasing integer. The names are
unique but not stable across evaluation orders (lazy forcing may invoke
`gensym` in any sequence); this is intentional --- `gensym` guarantees
uniqueness, not reproducibility.

Default hygiene via scope sets (Flatt 2016) is a later upgrade path
(macros Phase 3). Variables introduced by the macro template would be
scoped to the macro definition site; variables from the call site scoped to
the caller. An intentional hygiene escape hatch (allowing a macro to inject
bindings into the caller's scope) is deferred pending observation of
real-world macro usage patterns --- it creates an unrestricted scope
injection surface for library macros and is not needed for any example in
this document.

### AST-as-Dict Representation

The AST enum (`Expr`) must be projected into tinct dicts with a stable
schema. Changes to the AST break existing macros, so the representation
should be versioned or use an abstraction layer. Each AST node becomes a
dict with a `type` key discriminator. This representation should:

- **Use string `type` discriminator** --- `[type: call ...]`,
  `[type: var ...]`, etc. This is the tagged-union convention already
  used by `try` results (`[ok: ...]` / `[err: ...]`).
- **Mirror the `Expr` enum** --- one dict shape per `Expr` variant, with
  fields matching the Rust struct fields.
- **Include spans** --- macro-generated nodes carry the expansion site's
  span for error reporting.
- **Be versionable** --- add a `version` field to the root if schema
  changes are needed later.

See `doc/whatif/ast-schema.md` for the canonical AST dict schema
— all consumers (formatter, quasiquoting, macros) share one definition.
See `doc/whatif/quasiquoting.md` for the `quote`/`unquote` mechanism.

### Compile-Time Evaluation

Macro bodies must execute during expansion, before the main evaluation
pass. This requires a restricted evaluator (or reuse of the main evaluator)
that runs macro definitions eagerly. Since tinct is interpreted, this is
the same evaluator with a separate entry point --- not a distinct
compilation phase.

**Lazy evaluation tension:** macros need their arguments as *unevaluated
AST*, not as lazy thunks. A macro call site `[when pred body]` must pass
the *syntax* `pred` and `body`, not their *values*. This requires
special handling --- macro arguments bypass the normal evaluation model.

**Termination:** recursive macro expansion could loop. A depth limit plus
blackhole detection (analogous to the evaluator's InProgress sentinel)
prevents infinite expansion.

### Error Reporting

Macro-generated AST nodes must carry both the expansion source span and
the original macro call span. Pombrio & Krishnamurthi (2014) resugaring
maps errors in expanded code back to the surface syntax the user wrote.
Without this, errors in macro-generated code point to generated AST, not
original source --- a significant usability degradation.

### Interaction with `include`

Macros defined in an included file should be available to the includer.
This works naturally if `include` evaluates the file (making macro
definitions available) before the includer's expansion phase. This is the
same ordering Racket uses: `require` runs the required module's
compile-time code before expanding the requiring module.

## What Would Change

### Parser / Grammar

**Current:** Keywords `call`, `fn`, `type` are recognized as special forms.
No macro definition or invocation syntax exists.

**Proposed:** Add `defmacro` keyword. `[defmacro name [params] body]`
produces a new AST node (`Expr::DefMacro`). Macro invocations are
syntactically identical to function calls --- the expander distinguishes
them by name lookup against registered macros.

**Impact:** Moderate. One new keyword (`defmacro`), one new AST variant.
No change to expression parsing --- macro invocation reuses existing syntax.

### AST

**Current:** `Expr` enum represents all tinct expressions. No
representation of code-as-data.

**Proposed:** Add `Expr::DefMacro` and `Expr::Quote`/`Expr::Unquote`
variants (the latter from `doc/whatif/quasiquoting.md`). Define a stable
`Expr -> Value::Dict` projection (`ast_to_dict`) and its inverse
(`dict_to_ast`).

**Impact:** Major. New AST variants, plus a stable serialization schema
that becomes a public API surface. Schema changes break existing macros.

### Evaluator

**Current:** Single-pass evaluation. No expansion phase.

**Proposed:** Insert macro expansion phase between parsing and type
checking: `parse -> expand_macros -> typecheck -> eval`. The expander
walks the AST top-down, calling macro functions when it encounters
registered forms, and recurses into the expansion result until no macros
remain (fixpoint). A depth limit prevents infinite expansion.

**Impact:** Major. New pipeline phase. Macro functions run in a separate
evaluation context (eagerly, before main evaluation). The expander must
handle fixpoint expansion, termination detection, and error propagation.

### Type Checker

**Current:** Operates on AST post-desugar.

**Proposed:** Operates on AST post-expansion. Macro-generated code must
type-check like hand-written code. No special type rules for macros ---
expansion is transparent to the type system.

**Impact:** Minor. The type checker sees expanded AST, which is ordinary
tinct. No new type rules needed.

### Lazy Evaluation

**Current:** All expressions are lazy (call-by-need).

**Proposed:** Macro arguments are *not* evaluated --- they are quoted
(converted to AST dicts) and passed as data. This is a fundamental
departure from normal evaluation: macro call sites bypass lazy evaluation
for their arguments. The expanded result re-enters normal lazy evaluation.

**Impact:** Moderate. Macro call sites have different evaluation semantics
than function call sites. The expander must distinguish the two.

### Error System

**Current:** Errors carry source spans from the parser.

**Proposed:** Macro-generated AST carries dual spans: the expansion site
(where the macro was invoked) and the generated site (the macro body that
produced the code). Error messages should show both locations (Pombrio &
Krishnamurthi 2014 resugaring).

**Impact:** Moderate. Span representation may need to support chains
(macro A expands to macro B expands to code). Error formatting must handle
multi-location spans.

## Phased Adoption

### Phase 1: AST-as-Dict Infrastructure

Implement `ast_to_dict_expr`, `ast_to_dict`, and `dict_to_ast` per the
canonical schema in `doc/whatif/ast-schema.md`. This phase is shared with
`doc/whatif/quasiquoting.md` Phase 1 and `doc/whatif/tinct-hosted-formatter.md`
Phase 1 — all three are unblocked by the same `src/ast_dict.rs` implementation.

### Phase 2: `defmacro` and Basic Expansion

Add the `defmacro` keyword. Implement the expansion loop: walk AST
top-down, call macro functions on quoted arguments, replace with
expansion result. Support `gensym` for fresh names. Depth limit for
termination.

### Phase 3: Hygiene and Error Reporting

Add scope sets or context annotations for automatic hygiene. Implement
dual-span tracking for macro-generated AST. Resugaring for error messages.

### Phase 4: Integration

Connect with quasiquoting (`doc/whatif/quasiquoting.md`) for ergonomic
macro bodies. Connect with `include` for cross-file macro definitions.
Port `_` desugaring from hardcoded Rust to a tinct-defined macro.

### Prerequisites

- **Phase 1:** Stable `Expr` enum --- changes to AST node types break
  the schema. Grammar should be stable before committing to an AST dict
  format.
- **Phase 2:** Phase 1 complete. Quasiquoting
  (`doc/whatif/quasiquoting.md`) Phase 2 (the `quote` special form) is
  strongly recommended but not required --- macros can construct AST dicts
  manually.
- **Phase 3:** Phase 2 complete. Pattern matching
  (`doc/whatif/pattern-matching.md`) Phase 3 (dict destructuring) makes
  macro bodies much more ergonomic.
- **Phase 4:** All prerequisite whatif features implemented.

### Trigger

- **`_` desugaring sprint completes** --- the hardcoded pass provides
  the baseline semantics; the macro system generalizes it
- **A second syntactic desugaring is needed** (e.g., string interpolation,
  `let` bindings, pattern matching) --- confirms the pattern
- **Users request domain-specific syntax** --- validates the
  user-extensibility value proposition

## References

**Macro systems:**
- Kohlbecker, E., Friedman, D.P., Felleisen, M. & Duba, B. (1986).
  "Hygienic macro expansion." In *LFP '86*, pp. 151--161. ACM. ---
  Original hygiene algorithm (KFFD). Time-stamped renaming to prevent
  accidental capture.
- Clinger, W.D. & Rees, J. (1991). "Macros that work." In *POPL '91*,
  pp. 155--162. ACM. --- Unified hygienic expansion combining KFFD
  renaming with the R4RS `syntax-rules` pattern language. Linear-time
  algorithm. (Note: syntactic closures are a different mechanism,
  introduced in Bawden & Rees 1988.)
- Dybvig, R.K., Hieb, R. & Bruggeman, C. (1993). "Syntactic abstraction
  in Scheme." *Lisp and Symbolic Computation*, 5(4), 295--326. ---
  `syntax-case`: full procedural power with automatic hygiene via syntax
  objects.
- Flatt, M. (2002). "Composable and compilable macros: you want it when?"
  In *ICFP '02*, pp. 72--83. ACM. --- Phase separation via modules.
  Explicit compile-time/runtime phases.
- Adams, M.D. (2015). "Towards the essence of hygiene." In *POPL '15*,
  pp. 457--469. ACM. --- Algorithm-independent formal definition of
  hygiene as a property.
- Flatt, M. (2016). "Binding as sets of scopes." In *POPL '16*,
  pp. 705--717. ACM. --- Scope sets: simpler, more uniform hygiene model
  replacing rename-based approaches. Candidate for Phase 3 hygiene.
- Bawden, A. & Rees, J. (1988). "Syntactic closures." In *LFP '88*,
  pp. 86--95. ACM. --- Introduces syntactic closures: first-class
  representations of syntactic environments that allow controlled
  variable capture. A distinct hygiene mechanism from KFFD renaming and
  Clinger & Rees pattern-based expansion.
- Ballantyne, M., King, A. & Felleisen, M. (2020). "Macros for
  domain-specific languages." *OOPSLA '20*. --- Surface-to-core
  architecture for DSL macros. Relevant to tinct's "one language"
  philosophy.

**Lazy evaluation and macro need:**
- Launchbury, J. (1993). "A natural semantics for lazy evaluation." In
  *POPL '93*, pp. 144--154. ACM. --- Formal semantics showing call-by-need
  provides deferred evaluation without macros.
- Mitchell, N. (2007). "Haskell and macros." Blog post. --- Argues
  laziness makes macros "probably minimal" in Haskell.

**Error reporting through macros:**
- Pombrio, J. & Krishnamurthi, S. (2014). "Resugaring: lifting
  evaluation sequences through syntactic sugar." In *PLDI '14*,
  pp. 361--371. ACM. --- Formalizes how to present desugared evaluation
  steps in terms of surface syntax. The underlying principle --- that
  expanded code should be traceable back to the user's original source
  --- motivates dual-span tracking for macro error messages.
- Pombrio, J. & Krishnamurthi, S. (2015). "Hygienic resugaring of
  compositional desugaring." In *ICFP '15*, pp. 75--87. ACM. ---
  Extends the 2014 resugaring framework to handle compositional (nested)
  desugaring hygienically. Directly applicable to nested macro expansion
  provenance.
- Krishnamurthi, S. (2012). *Programming Languages: Application and
  Interpretation.* --- parse -> desugar -> typecheck -> eval pipeline that
  tinct's expansion phase follows.

**Precedent implementations:**
- Elixir `defmacro` --- the closest practical model. AST is 3-tuples,
  `quote`/`unquote` convert between code and data. Hygienic by default
  with `var!` escape hatch.
- Racket `syntax-case` --- full procedural power with automatic hygiene
  via syntax objects.
- Common Lisp `defmacro` --- unhygienic, full power, decades of
  production use.
