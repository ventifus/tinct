# What If: Desugaring as Macros

## Status

Proposal — not approved for implementation.

## Problem

tinct has multiple syntactic sugar mechanisms implemented as separate, disconnected systems:

1. **`$_` implicit lambda** — hardcoded in `eval.rs:66-71` (being moved to `src/desugar.rs` as a pre-typecheck AST pass)
2. **Stdlib sugar** — `->`, `when`, `unless`, `cond`, `>=`, `<=`, `>`, `compose` — ordinary lazy functions in `stdlib/prelude.llt`
3. **Parser-level sugar** — access chains (`$data.name` → `DotAccess`), keyword forms (`call`, `fn`, `type`)
4. **Runtime scoping** — document pipeline (`---`/`$$`), dict letrec

Each new piece of sugar requires a different implementation strategy: Rust AST rewrite for `$_`, stdlib function for `when`, grammar rule for access chains. There is no unified mechanism for defining syntactic transformations, and users cannot define their own.

The question: can all desugaring be unified under a macro system — one where syntactic sugar is defined as macro rules rather than hardcoded transformations?

## Current State

### What Is Actually Desugaring?

Not everything called "sugar" is the same kind of transformation. A useful taxonomy:

| Category | Examples | Level | User-extensible? |
|----------|----------|-------|-------------------|
| **Syntactic desugaring** | `$_` → `[fn [_] ...]` | AST → AST | No (Rust code) |
| **Functional sugar** | `->`, `when`, `>=` | Function composition | Yes (stdlib) |
| **Lexical sugar** | `$data.name` → `DotAccess(...)` | Tokenizer/parser | No (grammar rule) |
| **Semantic scoping** | `---`/`$$`, dict letrec | Evaluator | No (core semantics) |

Only the first category — syntactic desugaring — is a candidate for macros. Functional sugar already works fine as lazy functions. Lexical sugar and semantic scoping are below the level macros operate.

Today, `$_` is the only syntactic desugaring. String interpolation (DESIGN.md line 193, "if added") would be the second. The question is whether a general macro system is worth building for these, or whether hardcoded AST passes are sufficient.

### Why Laziness Reduces Macro Need

In strict languages, macros are essential for:
- **Short-circuit evaluation**: `and(a, b)` evaluates both args; you need a macro for `(and a b)` that skips `b` when `a` is false.
- **Conditional execution**: `if-then-else` must be a special form or macro, not a function.
- **Deferred computation**: Avoiding work that might not be needed.

tinct is lazy. All of these work as ordinary functions:
```lisp
# These are functions in stdlib/prelude.llt, not macros
when:   [fn [pred body] [call $if $pred $body []]]
unless: [fn [pred body] [call $if $pred [] $body]]
and:    [fn [a b] [call $if $a $b false]]
or:     [fn [a b] [call $if $a true $b]]
```

Because tinct uses call-by-need evaluation, `[call $when false [call $expensive]]` never forces `$expensive`. Laziness gives you the main benefit of macros — deferred evaluation — for free.

Nix and Jsonnet demonstrate this at scale: neither has macros, and laziness covers most use cases. Nix's module system (`lib.mkIf`, `lib.mkOverride`) is built entirely from lazy functions returning tagged attribute sets.

### Where Laziness Falls Short

Macros provide things lazy functions cannot:

1. **New syntax** — Functions cannot change how code is parsed. `$_` creates a syntax that doesn't look like a function call.
2. **Compile-time computation** — Functions defer to runtime. Macros can compute at expansion time, eliminating overhead.
3. **Structural transformation** — Functions receive values. Macros receive AST and can restructure it (reorder, duplicate, elide subexpressions based on syntactic shape).
4. **Binding introduction** — Functions cannot introduce new variable bindings in the caller's scope. Macros can expand to forms that bind names.
5. **Zero-cost abstraction** — Every function call in tinct creates a thunk. A macro that expands to inline code avoids thunk creation.

## Approaches

### Approach A: Declarative Rewrite Rules

Define macros as pattern → template rewrite rules in tinct syntax, similar to Scheme's `syntax-rules`. Rules are declared in source files and expanded after parsing, before type checking.

**Syntax:**
```lisp
# Pattern → template rules
[rule when [when $pred $body]
  [call $if $pred $body []]]

[rule unless [unless $pred $body]
  [call $if $pred [] $body]]

# $_ as a rule (simplified — real version needs DIRECT predicate)
[rule implicit-lambda [call $f $_ $args...]
  [fn [_] [call $f $_ $args...]]]
```

**Pipeline:**
```
source → parse → expand_rules (until fixpoint) → typecheck → eval
```

**How it works:**
- Rules are `[pattern] → [template]` pairs where `$name` in the pattern binds a metavariable and `$name` in the template substitutes it
- `$args...` binds a variadic sequence (like Scheme's `...` pattern)
- Expansion is bottom-up: innermost forms expand first (unlike Racket's top-down)
- Rules are hygienic by construction: pattern variables are the only binding mechanism, and templates can only reference pattern-bound variables or globally-scoped names
- Expansion terminates when no rules match (fixpoint). A depth limit prevents infinite expansion

**Hygiene model:**
- **Automatic**: pattern variables bind at the macro call site; template references resolve at the macro definition site (following Kohlbecker et al. 1986)
- **No escape hatch**: unlike Racket's `datum->syntax` or Elixir's `var!`, there is no way to break hygiene. This limits power but eliminates an entire class of bugs
- Since tinct has no module system yet, "definition site" means "file scope where the rule appears"

**Pros:**
- Declarative — rules are data, easy to read and reason about
- Hygienic by construction — no variable capture bugs
- User-extensible — users can define new sugar in tinct files
- Formally specifiable — rewrite rules have clear semantics (term rewriting systems, Baader & Nipkow 1998)
- Fits tinct's data-first philosophy — rules are just pattern matching on structure

**Cons:**
- Limited power — cannot do conditional expansion based on values, recursive decomposition, or computed templates
- Pattern language needs its own grammar — `$args...` variadic, nested patterns, literal matching vs variable binding
- Confluence is hard — multiple rules may match the same form, and the order of expansion matters. Proving confluence requires checking all rule pairs (Knuth-Bendix completion)
- `$_` desugaring doesn't fit cleanly — the DIRECT predicate and depth-based shadowing are more complex than a simple pattern → template rule
- No compile-time computation — rules are purely structural, cannot evaluate expressions during expansion

**Precedent:**
- Scheme `syntax-rules` (R5RS) — the direct inspiration. Declarative, hygienic, but limited in power (no procedural macros). Languages that adopted `syntax-rules` eventually added `syntax-case` for the cases where declarative rules weren't enough.
- Rust `macro_rules!` — pattern-based, partially hygienic. More powerful than `syntax-rules` (repetition, multiple arms, fragment specifiers) but still limited enough that procedural macros were added later.

**Implementation complexity:** Medium. Requires: pattern matching engine, template substitution, fixpoint expansion loop, hygiene scope tracking, grammar extensions for `[rule ...]`.

### Approach B: Procedural AST Macros — Data Transforms on Code

Macros are tinct functions that receive AST-as-data and return AST-as-data. This is the "code is data" approach: tinct values (dicts, lists, strings) represent AST nodes, and macro expansion is just data transformation — the same thing tinct already does.

**Syntax:**
```lisp
# AST is represented as tinct dicts
# [call $f $x $y] is the dict:
#   [type: call  fn: [type: var  name: f]  args: [[type: var  name: x] [type: var  name: y]]]

# A macro is a function from AST-dict to AST-dict
[defmacro when [pred-ast body-ast]
  [type: call
   fn: [type: var  name: if]
   args: [$pred-ast  $body-ast  [type: literal  value: []]]]]

# Or with quote/unquote syntax sugar:
[defmacro when [pred body]
  [quote [call $if [unquote $pred] [unquote $body] []]]]
```

**Pipeline:**
```
source → parse → quote_macros → expand (call macro fns on quoted AST) → typecheck → eval
```

**How it works:**
- `[defmacro name [params] body]` registers a compile-time function
- When `[name arg1 arg2 ...]` appears in source, the parser quotes the arguments (converts AST to tinct dicts) and calls the macro function with the quoted forms
- The macro function returns a tinct dict representing the expanded AST
- The expander converts the dict back to AST and continues expansion
- `quote` converts code to its AST-dict representation; `unquote` splices values into quoted code

**Hygiene model:**
- **Not automatic** — macro authors must manage variable names
- `[gensym]` builtin provides fresh unique names for introduced bindings
- Convention over enforcement: macros should use `$gensym` for internal bindings
- This matches Template Haskell and early Common Lisp — hygiene is opt-in

**The tinct-specific insight:** tinct is a data transformation language. Its core operation is transforming structured data (dicts, lists) into other structured data. Procedural AST macros are literally the same operation applied to the language's own AST. A tinct macro author uses the same `$map`, `$filter`, `$get` functions they already know, applied to AST-dicts instead of user data.

**Pros:**
- Full power — arbitrary computation during expansion
- Uses tinct's own data model — no separate pattern language, no new concepts
- "One language" philosophy — sugar is defined in tinct, not Rust
- Naturally composable — macros are functions, compose like functions
- User-extensible DSLs — users can create domain-specific syntax
- `$_` desugaring fits: write the DIRECT predicate and WRAP logic as a tinct function operating on AST-dicts

**Cons:**
- Unhygienic by default — variable capture bugs are possible
- Phase separation complexity — macro functions must be evaluated at compile time, but tinct has no compilation phase (it's interpreted). Either: (a) macros are evaluated eagerly in a separate pass, breaking laziness assumptions, or (b) macro expansion is deferred, complicating the pipeline
- Error reporting degrades — errors in macro-generated code point to generated AST, not original source. Requires Pombrio & Krishnamurthi (2014) resugaring to map back
- AST-as-dict representation must be defined and stabilized — any change to the AST enum breaks existing macros
- **Lazy evaluation tension**: macros need their arguments as *unevaluated AST*, not as lazy thunks. A macro call site `[when $pred $body]` must pass the *syntax* `$pred` and `$body`, not their *values*. This requires special handling — macro arguments bypass the normal evaluation model
- Termination: recursive macro expansion could loop. Need depth limit + blackhole detection

**Precedent:**
- Elixir `defmacro` — the closest model. AST is 3-tuples, `quote`/`unquote` convert between code and data. Hygienic by default with `var!` escape hatch.
- Racket `syntax-case` — full procedural power with automatic hygiene via syntax objects
- Common Lisp `defmacro` — unhygienic, full power, decades of production use

**Implementation complexity:** High. Requires: AST-to-dict representation, `quote`/`unquote` forms, macro registration, compile-time evaluation environment, expansion loop, gensym, and either resugaring or span-threading for error reporting.

### Approach C: Hybrid — Declarative Rules + Escape to Procedural

Offer simple declarative rules (Approach A) as the default, with an escape hatch to procedural macros (Approach B) when rules aren't sufficient.

**Syntax:**
```lisp
# Simple sugar — declarative rule
[rule when [when $pred $body]
  [call $if $pred $body []]]

# Complex sugar — procedural macro
[defmacro cond [pairs-ast]
  # Recursively build nested if-else from pairs
  [call $cond-expand $pairs-ast]]
```

**Pros:**
- Right tool for each job — simple sugar is simple, complex sugar has full power
- Gradual adoption — start with rules, add procedural when needed
- Declarative rules are hygienic; procedural macros have explicit gensym

**Cons:**
- Two systems to learn, implement, and maintain
- Interaction between the two (rule vs. procedural expansion order, which wins on conflict) adds complexity
- Design surface area doubles

**Precedent:**
- Rust (`macro_rules!` + proc macros) — exactly this split. Works well in practice but the community acknowledges it's complex.
- Scheme (evolved from `syntax-rules` to `syntax-case`) — started declarative, added procedural later. The evolution suggests declarative alone isn't enough.

**Implementation complexity:** Very high — sum of A and B plus interaction rules.

## Analysis: What Would Each Approach Do For Current Sugar?

| Sugar | A (Rules) | B (Procedural) | C (Hybrid) |
|-------|-----------|-----------------|------------|
| `$_` implicit lambda | Doesn't fit well — DIRECT predicate + depth shadowing too complex for simple rules | Fits — write DIRECT/WRAP as tinct function | Rule attempt, fall back to procedural |
| `->` threading | Rule: `[-> $x $f $rest...] → [call $f [-> $x $rest...]]` | Procedural: recursive expansion | Rule |
| `when`/`unless` | Rule: `[when $p $b] → [call $if $p $b []]` | Procedural: trivial | Rule |
| `>=`/`<=`/`>` | Rule: `[>= $a $b] → [call $not [call $< $a $b]]` | Procedural: trivial | Rule |
| String interpolation | Fits if pattern language supports string literals | Fits | Rule |
| User-defined DSL | Simple DSLs only | Full DSLs | Both |

**Key observations:**
- Approach A can't handle `$_` — the DIRECT predicate, depth shadowing, and func-position exclusion require recursive AST traversal with contextual state, not flat pattern matching
- Approach B handles everything including `$_`, and fits tinct's data-transformation philosophy
- Approach C adds complexity for marginal benefit — Scheme's evolution from `syntax-rules` to `syntax-case` shows declarative alone isn't enough, so starting with procedural avoids the two-system tax

## Interaction with Lazy Evaluation

Laziness and macros serve overlapping but distinct purposes:

**What laziness already provides:** Deferred evaluation. tinct's `when`, `unless`, `and`, `or`, `->` work as ordinary lazy functions — `[call $when false [call $expensive]]` never forces `$expensive`. This covers the use case that motivates macros in strict languages (short-circuit evaluation, conditional execution).

**What macros provide beyond laziness:**
- **Binding introduction**: `[let x 10 body]` → `[call [fn [x] body] 10]`. Functions cannot introduce new variable bindings in the caller's scope.
- **Structural transformation**: Reorganizing AST shapes that functions can't express — `$_`'s WRAP behavior wraps an entire expression based on what's inside it, which no function call can do.
- **Compile-time validation**: Checking that macro arguments have the right syntactic shape before evaluation.
- **New syntax**: Creating forms that don't look like function calls (e.g., `$_` creates implicit lambdas from bare expressions).
- **Zero-cost abstraction**: A macro that inlines code avoids thunk creation entirely. For strict operations, this eliminates per-call overhead.

The overlap means many stdlib functions (`when`, `>=`, `compose`) would NOT benefit from becoming macros — they work fine as lazy functions. Macros are for the cases laziness can't cover: binding, structural transformation, and new syntax.

## `$_` as a Procedural Macro

The acid test for the macro system: can `$_` desugaring be expressed as a user-definable macro rather than a hardcoded Rust pass?

The `$_` transformation requires (per DESIGN.md §$_ Desugaring):

1. **DIRECT predicate** — identify `VarRef("_")` or access chains rooted at `$_` (e.g., `$_.name`, `$_[0]`)
2. **Top-down WRAP check** — examine raw children of Call, Dict, DotAccess, BracketAccess, RangeAccess before recursing
3. **Func-position exclusion** — `$_` in function position of a Call does NOT trigger wrapping
4. **Depth-based shadowing** — inside `[fn [_] ...]`, the `_` parameter shadows `$_` desugaring
5. **Lambda wrapping** — wrap the containing expression in `[fn [_] expr]` with span preservation

As a procedural macro operating on AST-dicts:

```lisp
# DIRECT predicate: is this node $_ or an access chain rooted at $_?
direct?: [fn [node]
  [call $or
    [call $and [call $= $node.type var] [call $= $node.name _]]
    [call $and [call $= $node.type dot-access]
               [call $direct? $node.target]]
    [call $and [call $= $node.type bracket-access]
               [call $direct? $node.target]]]]

# Check if any child of a node is DIRECT
has-direct-child?: [fn [node]
  [call $cond [
    [[call $= $node.type call]
      [call $any? $direct? $node.args]]
    [[call $= $node.type dict]
      [call $any? [fn [entry] [call $direct? $entry.value]] $node.entries]]
    [true
      [call $direct? $node]]
  ]]]

# The macro: wrap expression in [fn [_] expr] if it has a DIRECT child
[defmacro desugar-underscore [expr]
  [call $if [call $has-direct-child? $expr]
    [quote [fn [_] [unquote $expr]]]
    $expr]]
```

This demonstrates that a procedural macro system with AST-as-dict is powerful enough to express `$_`. The DIRECT predicate, child inspection, and conditional wrapping all use ordinary tinct functions (`$any?`, `$=`, `$cond`) applied to AST structure.

Depth-based shadowing would be handled by the macro expander itself: when expanding inside a `[fn [_] ...]` body, `$_` is bound and the macro does not fire — the same scoping rules that apply to any hygienic macro.

## Recommendation

**Approach B (Procedural AST Macros).**

### Rationale

1. **Powerful enough for `$_`.** As shown above, the DIRECT predicate, WRAP logic, and conditional wrapping are all expressible as tinct functions operating on AST-dicts. A macro system that can't handle the hardest existing desugaring doesn't pay for itself.

2. **Fits tinct's philosophy.** tinct transforms structured data. Procedural AST macros are the same operation applied to the language's own AST. Macro authors use `$map`, `$filter`, `$get` on AST-dicts — tools they already know.

3. **User-extensible.** Users can define domain-specific sugar, binding forms, and structural transformations in tinct files. `$include` provides file-level composition until a module system exists.

4. **Proven model.** Elixir's `defmacro` with `quote`/`unquote` and default hygiene has served production use for over a decade. Racket's `syntax-case` provides the theoretical foundation.

### Design Considerations

1. **AST-as-dict representation.** The AST enum (`Expr`) must be projected into tinct dicts with a stable schema. Changes to the AST break existing macros, so the representation should be versioned or use an abstraction layer. Each AST node becomes a dict with a `type` key discriminator.

2. **Expansion phase.** Macros expand after parsing, before type checking: `source → parse → expand_macros → typecheck → eval`. The expander walks the AST top-down, calling macro functions when it encounters registered forms, and recurses into the expansion result until no macros remain (fixpoint). A depth limit prevents infinite expansion.

3. **Compile-time evaluation.** Macro bodies must execute during expansion, before the main evaluation pass. This requires a restricted evaluator (or reuse of the main evaluator) that runs macro definitions eagerly. Since tinct is interpreted, this is the same evaluator with a separate entry point — not a distinct compilation phase.

4. **Hygiene.** Default hygiene via scope sets (Flatt 2016) or context-annotated variables (Elixir model). Variables introduced by the macro template are scoped to the macro definition site; variables from the call site are scoped to the caller. `gensym` provides fresh names when hygiene must be broken intentionally.

5. **Error reporting.** Macro-generated AST nodes must carry both the expansion source span and the original macro call span. Pombrio & Krishnamurthi (2014) resugaring maps errors in expanded code back to the surface syntax the user wrote.

6. **Interaction with `$include`.** Macros defined in an included file should be available to the includer. This works naturally if `$include` evaluates the file (making macro definitions available) before the includer's expansion phase. This is the same ordering Racket uses: `require` runs the required module's compile-time code before expanding the requiring module.

## Trigger

Conditions that would make this the right next step:

- **`$_` desugaring sprint completes** — the hardcoded pass provides the baseline semantics; the macro system generalizes it
- **A second syntactic desugaring is needed** (e.g., string interpolation, `let` bindings, pattern matching) — confirms the pattern
- **Users request domain-specific syntax** — validates the user-extensibility value proposition

## References

**Macro systems:**
- Kohlbecker, E., Friedman, D.P., Felleisen, M. & Duba, B. (1986). Hygienic macro expansion. In *LFP '86*, pp. 151–161. ACM. — Original hygiene algorithm (KFFD). Time-stamped renaming to prevent accidental capture.
- Clinger, W.D. & Rees, J. (1991). Macros that work. In *POPL '91*, pp. 155–162. ACM. — Unified hygienic expansion with syntactic closures. Linear-time algorithm.
- Dybvig, R.K., Hieb, R. & Bruggeman, C. (1993). Syntactic abstraction in Scheme. *Lisp and Symbolic Computation*, 5(4), 295–326. — `syntax-case`: full procedural power with automatic hygiene via syntax objects.
- Flatt, M. (2002). Composable and compilable macros: you want it when? In *ICFP '02*, pp. 72–83. ACM. — Phase separation via modules. Explicit compile-time/runtime phases.
- Adams, M.D. (2015). Towards the essence of hygiene. In *POPL '15*, pp. 457–469. ACM. — Algorithm-independent formal definition of hygiene as a property.
- Flatt, M. (2016). Binding as sets of scopes. In *POPL '16*, pp. 705–717. ACM. — Scope sets: simpler, more uniform hygiene model replacing rename-based approaches.
- Ballantyne, M., King, A. & Felleisen, M. (2020). Macros for domain-specific languages. *OOPSLA '20*. — Surface-to-core architecture for DSL macros.

**Lazy evaluation and macro need:**
- Launchbury, J. (1993). A natural semantics for lazy evaluation. In *POPL '93*, pp. 144–154. ACM. — Formal semantics showing call-by-need provides deferred evaluation without macros.
- Mitchell, N. (2007). Haskell and macros. Blog post. — Argues laziness makes macros "probably minimal" in Haskell.

**Term rewriting (for Approach A):**
- Baader, F. & Nipkow, T. (1998). *Term Rewriting and All That*. Cambridge University Press. — Confluence, termination, and completion for rewrite systems.

**Already cited in DESIGN.md:**
- Pombrio & Krishnamurthi (2014) — resugaring through macro expansion
- Krishnamurthi (2012) — parse → desugar → typecheck → eval pipeline
