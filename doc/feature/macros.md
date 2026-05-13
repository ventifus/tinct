# Desugaring as Macros

## Overview

The macro system unifies tinct's syntactic sugar under a single, user-extensible mechanism. Macros are tinct functions that receive AST-as-data and return AST-as-data — "code is data." Macro expansion is data transformation using the same tools (`map`, `filter`, `get`) users already know.

- **User-extensible syntax** — domain-specific binding forms, control flow, and structural transformations defined in tinct, not Rust
- **Unified desugaring** — all syntactic sugar (`_`, string interpolation, future constructs) expressed as macro rules rather than hardcoded passes
- **"One language" philosophy** — sugar is defined in tinct using tinct
- **Zero-cost abstraction** — macros that expand to inline code avoid thunk creation, eliminating per-call overhead for strict operations
- **Self-hosting path** — reduces the Rust surface area by expressing syntactic transformations in tinct itself

`defmacro`, `macro-hygiene`, and `macro-integration` are implemented. See TODO.md for open follow-up tasks.

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

Because tinct uses call-by-need evaluation, `[when false [expensive]]` never forces `expensive`. Laziness gives you the main benefit of macros — deferred evaluation — for free.

Nix and Jsonnet demonstrate this at scale: neither has macros, and laziness covers most use cases. Nix's module system (`lib.mkIf`, `lib.mkOverride`) is built entirely from lazy functions returning tagged attribute sets.

### Where Laziness Falls Short

Macros provide things lazy functions cannot:

1. **New syntax** — Functions cannot change how code is parsed. `_` creates a syntax that doesn't look like a function call.
2. **Compile-time computation** — Functions defer to runtime. Macros compute at expansion time, eliminating overhead.
3. **Structural transformation** — Functions receive values. Macros receive AST and can restructure it (reorder, duplicate, elide subexpressions based on syntactic shape).
4. **Binding introduction** — Functions cannot introduce new variable bindings in the caller's scope. Macros can expand to forms that bind names.
5. **Zero-cost abstraction** — Every function call in tinct creates a thunk. A macro that expands to inline code avoids thunk creation.

The overlap means many stdlib functions (`when`, `>=`, `compose`) do NOT benefit from becoming macros — they work fine as lazy functions. Macros are for the cases laziness can't cover: binding, structural transformation, and new syntax.

## `_` as the Acid Test

The acid test for the macro system: can `_` desugaring be expressed as a user-definable macro rather than a hardcoded Rust pass?

The `_` transformation requires (per doc/04-functions.md `_` Desugaring):

1. **DIRECT predicate** — identify `VarRef("_")` or access chains rooted at `_` (e.g., `_.name`, `_[0]`)
2. **Top-down WRAP check** — examine raw children of Call, Dict, DotAccess, and Pipe before recursing (bracket access and range access were removed in access-pipeline-phase2)
3. **Func-position exclusion** — `_` in function position of a Call does NOT trigger wrapping
4. **Depth-based shadowing** — inside `[fn [_] ...]`, the `_` parameter shadows `_` desugaring
5. **Lambda wrapping** — wrap the containing expression in `[fn [_] expr]` with span preservation

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

Depth-based shadowing is handled by the macro expander itself: when expanding inside a `[fn [_] ...]` body, `_` is bound and the macro does not fire — the same scoping rules that apply to any hygienic macro.

## Design

Macros are tinct functions that receive AST-as-data and return AST-as-data. tinct values (dicts, lists, strings) represent AST nodes, and macro expansion is data transformation — the same thing tinct already does.

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

- **Not automatic** — macro authors must manage variable names
- `[gensym]` builtin provides fresh unique names for introduced bindings
- Convention over enforcement: macros should use `gensym` for internal bindings
- This matches Template Haskell and early Common Lisp — hygiene is opt-in

`gensym` names use a prefix containing `:` (a character forbidden in bare words), making collision structurally impossible: a user cannot write `:gensym:0` as a bare-word identifier in source. Names have the form `:gensym:N` where N is a monotonically increasing integer. The names are unique but not stable across evaluation orders (lazy forcing may invoke `gensym` in any sequence); this is intentional — `gensym` guarantees uniqueness, not reproducibility.

Default hygiene via scope sets (Flatt 2016) is supported. Variables introduced by the macro template are scoped to the macro definition site; variables from the call site are scoped to the caller. An intentional hygiene escape hatch (allowing a macro to inject bindings into the caller's scope) is not provided — it creates an unrestricted scope injection surface for library macros and is not needed for any example in this document.

### AST-as-Dict Representation

The AST enum (`Expr`) is projected into tinct dicts with a stable schema. Changes to the AST break existing macros, so the representation is versioned via a `schema-version` field. Each AST node becomes a dict with a `type` key discriminator. This representation:

- **Uses string `type` discriminator** — `[type: call ...]`, `[type: var ...]`, etc. This is the tagged-union convention already used by `try` results (`[ok: ...]` / `[err: ...]`).
- **Mirrors the `Expr` enum** — one dict shape per `Expr` variant, with fields matching the Rust struct fields.
- **Includes spans** — macro-generated nodes carry the expansion site's span for error reporting.
- **Is versionable** — a `version` field on the root if schema changes are needed later.

See `doc/feature/ast-schema.md` for the canonical AST dict schema — all consumers (formatter, quasiquoting, macros) share one definition.

### Compile-Time Evaluation

Macro bodies execute during expansion, before the main evaluation pass. This requires a restricted evaluator (or reuse of the main evaluator) that runs macro definitions eagerly. Since tinct is interpreted, this is the same evaluator with a separate entry point — not a distinct compilation phase.

**Lazy evaluation tension:** macros need their arguments as *unevaluated AST*, not as lazy thunks. A macro call site `[when pred body]` passes the *syntax* `pred` and `body`, not their *values*. Macro arguments bypass the normal evaluation model.

**Termination:** recursive macro expansion could loop. A depth limit plus blackhole detection (analogous to the evaluator's InProgress sentinel) prevents infinite expansion.

### Error Reporting

Macro-generated AST nodes carry both the expansion source span and the original macro call span. Pombrio & Krishnamurthi (2014) resugaring maps errors in expanded code back to the surface syntax the user wrote. Without this, errors in macro-generated code point to generated AST, not original source — a significant usability degradation.

### Interaction with `include`

Macros defined in an included file are available to the includer. This works because `include` evaluates the file (making macro definitions available) before the includer's expansion phase. This is the same ordering Racket uses: `require` runs the required module's compile-time code before expanding the requiring module.

## Implementation

### Parser / Grammar

`src/parser.rs` gains a `defmacro` keyword. `[defmacro name [params] body]` produces a new AST node (`Expr::DefMacro`). Macro invocations are syntactically identical to function calls — the expander distinguishes them by name lookup against registered macros. One new keyword (`defmacro`), one new AST variant; no change to expression parsing.

### AST

`src/parser.rs` (AST types) gains `Expr::DefMacro` and `Expr::Quote`/`Expr::Unquote` variants. `src/ast_dict.rs` defines a stable `Expr -> Value::Dict` projection (`ast_to_dict`) and its inverse (`dict_to_ast`). The schema is a public API surface — schema changes break existing macros.

### Evaluator

`src/eval.rs` gains a macro expansion phase between parsing and type checking: `parse -> expand_macros -> typecheck -> eval`. The expander walks the AST top-down, calling macro functions when it encounters registered forms, and recurses into the expansion result until no macros remain (fixpoint). A depth limit prevents infinite expansion. Macro functions run in a separate evaluation context (eagerly, before main evaluation).

### Type Checker

`src/typecheck.rs` operates on AST post-expansion. Macro-generated code type-checks like hand-written code. No special type rules for macros — expansion is transparent to the type system.

### Lazy Evaluation

Macro arguments are *not* evaluated — they are quoted (converted to AST dicts) and passed as data. This is a fundamental departure from normal evaluation: macro call sites bypass lazy evaluation for their arguments. The expanded result re-enters normal lazy evaluation. The expander distinguishes macro call sites from function call sites.

### Error System

Macro-generated AST carries dual spans: the expansion site (where the macro was invoked) and the generated site (the macro body that produced the code). Error messages show both locations (Pombrio & Krishnamurthi 2014 resugaring). Span representation supports chains (macro A expands to macro B expands to code).

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
