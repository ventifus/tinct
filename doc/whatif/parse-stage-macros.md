# What If: Parse-Stage Macros for tinct

**State:** Proposal

What would it take to let user-defined macros control how their argument positions are delivered — so the macro body itself, written in tinct, does all structural transformation work rather than toggling Rust-implemented flags?

## Current State

Tinct's macro system (`doc/whatif/macros.md`) operates post-parse: macros receive fully-formed `Expr` AST dicts and return AST dicts. The parser always produces a complete AST before any macro runs, using fixed rules for every bracket it encounters:

- **All brackets are parsed as expressions** — implied call, keyed dict, or type assertion based on content. `[x y]` in a macro arg position becomes `Call(VarRef("x"), [VarRef("y")])`, not the element list `[VarRef("x"), VarRef("y")]`.
- **Duplicate key detection uses bare-name identity** — `n@Int` and `n@String` are both field `"n"` → parse-time duplicate error, before any macro runs.
- **No receive-mode control** — macros cannot declare how their argument positions should be delivered; they receive whatever the parser produces.

These rules are correct for general-purpose dicts but make it impossible for user-defined macros to work with bracket forms that require different parse representations.

### The Core Problem

Consider the fn let-softening use case from `doc/whatif/unified-bindings.md`: a user writes `[fn [x@Int y@Float] body]` and wants it equivalent to `[fn [let x@Int y@Float] body]`. A post-parse macro cannot do this because `[x@Int y@Float]` has already been parsed as the implied call `Call(x@Int, [y@Float])` — the flat element sequence `[x@Int, y@Float]` is gone.

Or consider a dispatch macro with annotated keys:

```tinct
[dispatch result
  n@Int:    i"int: $n"
  n@String: i"str: $n"   # PARSE ERROR — duplicate key "n" before any macro runs
  _:        "other"]
```

Both problems have the same root: the parser's fixed rules transform bracket content before the macro gets a chance to see it differently.

### What's Missing

1. **Receive modes** — a way for a macro to declare that a specific argument position should be delivered in a different form (flat element list, raw token sequence, or full expression)
2. **Macro-body transformation** — the macro body itself, written in tinct, should do ALL structural transformation; no Rust-implemented flags or hardcoded transformation logic
3. **Context-sensitive key identity** — duplicate detection that uses full annotated expression equality instead of bare-name equality, for macros that use annotated keys

## Why Parse-Stage Macros Matter for tinct

**Macros that do real work.** The macro body is tinct code. It inspects the argument, decides what transformation to apply, and returns the new AST. The infrastructure provides receive modes and primitives; the logic lives in the macro.

**The let-softening path.** `doc/whatif/unified-bindings.md` requires `[let ...]` uniformly. Parse-stage macros let a future declaration make `[fn [x y] body]` equivalent to `[fn [let x y] body]` — with the transformation logic written entirely in tinct.

**User-defined language forms.** A user can implement their own `[my-match ...]`, `[my-for ...]`, or `[dispatch ...]` with the same syntactic flexibility as built-in forms.

**Unforeseen uses.** Any form where the parser's default interpretation loses structure that the macro needs to work with.

## Design

### `defparse-macro` — Declare a Parse-Stage Macro

```tinct
[defparse-macro name [arg: receive-mode  ...] body]
```

- `name` — the form name this macro handles (can be a keyword like `fn` or a user form)
- `[arg: receive-mode ...]` — declares how each argument position is delivered to the macro
- `body` — tinct code that receives the arguments and returns the new AST

The macro body runs in a **post-parse transformation pass**, after the file is parsed but before type-checking. It can use any tinct stdlib primitive available at that stage.

### Receive Modes

Each argument position has a **receive mode** that controls how the parsed bracket is delivered to the macro:

| Mode | What the macro receives | Example input | Example value |
|------|------------------------|---------------|---------------|
| `expr` | Fully parsed expression (default) | `[x y]` | `Call(VarRef("x"), [VarRef("y")])` |
| `flat-list` | Bracket elements as a sequence, no implied-call applied | `[x y]` | `[VarRef("x"), VarRef("y")]` |
| `tokens` | Raw token sequence within the bracket | `SELECT * FROM t` | `[Token("SELECT"), Token("*"), ...]` |

`flat-list` is the key mode: it preserves the bracket as a sequence of its elements without applying implied-call semantics. This lets the macro inspect and reshape the contents — for example, check if the first element is a `let` keyword and wrap if not.

`tokens` is the escape hatch for embedded DSLs that have their own syntax. The macro receives a raw token list and produces an `Expr`.

### Macro Body — All Logic in Tinct

The macro body uses tinct code and AST construction primitives to inspect arguments and produce the output. No Rust transformation logic. No flags.

**fn let-softening:**
```tinct
[defparse-macro fn [params: flat-list  body: expr]
  [if [let-decl? params]
    [list 'fn params body]                    # already has [let ...], pass through
    [list 'fn [cons 'let params] body]]]      # prepend let: [let x y]
```

- `params` arrives as `[VarRef("x"), VarRef("y")]` (flat element list)
- `[let-decl? params]` — checks if `params` already starts with the `let` keyword
- `[list 'fn params body]` — constructs the AST `[fn params body]`
- `[cons 'let params]` — prepends `let` to params, producing `[let x y]`

**class and type — identical pattern:**
```tinct
[defparse-macro class [tvars: flat-list  ...body: expr]
  [if [let-decl? tvars]
    [list 'class tvars ...body]
    [list 'class [cons 'let tvars] ...body]]]

[defparse-macro type [params: flat-list  body: expr]
  [if [let-decl? params]
    [list 'type params body]
    [list 'type [cons 'let params] body]]]
```

**case arm let-wrapping — macro handles the conditional logic:**
```tinct
[defparse-macro case [scrutinee: expr  ...arms: flat-list]
  [list 'case scrutinee
    ...[map [fn [let arm]
              [if [contains-structural-test? arm]
                arm                           # [let v: Ok]: body — don't touch
                [wrap-in-let arm]]]           # [n@Int]: body — safe to wrap
           arms]]]
```

The case macro does real work: it maps over arms, inspects each one for structural tests (`:` separator), and applies wrapping only where safe. The helper `contains-structural-test?` and `wrap-in-let` are tinct stdlib functions — not Rust implementations.

**Annotated-key dict — logic in the macro:**
```tinct
[defparse-macro dispatch [scrutinee: expr  arms: flat-list]
  # Arms received as a flat list of [key value] pairs with full-expression keys
  [list 'dispatch scrutinee
    ...[map [fn [let arm]
              [list [first arm] [second arm]]]  # reconstruct each arm
           arms]]]
```

The duplicate check with full-expression key identity is declared separately (see below). The macro itself handles dispatch logic.

### Key Identity — Separate from Transformation

Key identity (what counts as a duplicate key) is NOT a transformation — it's a parse-time rule about which bracket entries can coexist. It's declared separately from `defparse-macro`:

```tinct
[declare-key-identity dispatch  full-expression]
# Under full-expression identity: n@Int ≠ n@String ≠ n
```

| Key identity | Behavior |
|-------------|----------|
| `bare-name` | Default: `n@Int` and `n@String` both have key `"n"` → duplicate error |
| `full-expression` | `n@Int` and `n@String` are structurally distinct → both allowed |

`full-expression` identity means: duplicate detection compares the full parsed key node structurally, not just its extracted name. The macro then receives an arms list where each entry has a full-expression key — `Annotated("n", Simple("Int"))` vs `Annotated("n", Simple("String"))` are distinct.

Two `n@Int` entries are still a duplicate. Two `_` entries are still a duplicate. Only the annotation distinguishes them.

### AST Construction Primitives

The macro body uses tinct primitives to construct and inspect AST:

```tinct
# Inspection
[let-decl? expr]          # is this an Expr::LetDecl (a [let ...] form)?
[var-ref? expr]            # is this a bare identifier?
[annotated? expr]          # is this name@Type?
[literal? expr]            # is this a literal value?
[contains-structural-test? arm]  # does this arm contain a name: Constructor entry?

# Construction
[list 'sym a b c]          # construct AST list/form: [sym a b c]
[cons x xs]               # prepend x to list xs
[first xs]                # first element of list
[rest xs]                 # remaining elements
[quote sym]               # quote a symbol: 'fn, 'let, etc.

# Wrapping helpers (stdlib, implemented in tinct)
[wrap-in-let flat-list]   # produce [let ...flat-list]
[make-let-decl elements]  # produce Expr::LetDecl from element list
```

These are the same primitives available to post-parse macros, plus the parse-stage-specific ones (`let-decl?`, `make-let-decl`).

### Transformation Pass

Parse-stage macros run in a **transformation pass** between parsing and type-checking:

```
parse → [post-parse transformation pass] → type-check → eval
```

The transformation pass:
1. Walks the parsed AST
2. For each form `[name ...]` where `name` has a `defparse-macro` declaration:
   a. Re-deliver arguments in their declared receive modes
   b. Call the macro body with the re-delivered arguments
   c. Replace the original form with the macro's return value
3. The pass runs to fixpoint (handles macros that produce other macro calls)

**Bootstrapping:** `defparse-macro` declarations for `fn`, `class`, `type` live in `stdlib/syntax.llt` (a new file loaded before user code). The transformation pass only runs for declarations it has seen. stdlib syntax declarations are always available.

**Recursion guard:** the transformation pass tracks which forms it has already visited. A macro's output is not re-visited in the same pass unless it produces a new, unvisited form name.

### Scoping

`defparse-macro` and `declare-key-identity` follow normal tinct scoping rules: they are active for forms parsed within the same scope and nested scopes. An inner dict can declare a parse macro that overrides an outer one for that form name within that inner scope.

### Security Model

- **No lexer access** — `tokens` mode receives already-tokenized tokens; the lexer is always Rust-only
- **No arbitrary evaluation during parsing** — the transformation pass runs after parsing completes; macro bodies run with the stdlib evaluator, not a special parse-time evaluator
- **No cross-scope effects** — parse macro declarations are scoped; they do not affect other files

## What Would Change

### `src/expand.rs` — Parse-Stage Transformation Pass

**Current:** `expand.rs` handles post-parse macro expansion.

**Proposed:** After parsing, before type-checking, run the parse-stage transformation pass:
1. Scan for `defparse-macro` and `declare-key-identity` declarations (using the pre-existing pre-parse scan mechanism)
2. Walk the AST; for registered form names, re-deliver arguments in declared modes and call the macro body
3. Collect results and substitute in place

The pass uses the existing evaluator (`eval_source`) to run macro bodies, with stdlib loaded. The macro body is just a tinct function.

**Impact:** Moderate — new pass in the pipeline; interoperates with existing macro expansion.

### `src/parser.rs` — Receive Mode Support

**Current:** All brackets parsed as expressions (implied call, dict, etc.).

**Proposed:** `flat-list` and `tokens` modes require the parser to deliver bracket content differently. This is handled by the transformation pass re-processing the already-parsed AST node — for `flat-list`, the pass extracts the `entries` of a parsed dict/call and delivers them as a sequence. For `tokens`, a new `Expr::RawTokens` variant stores the token sequence.

For `declare-key-identity full-expression`: the pre-parse scan registers the form name, and the parser uses full-expression equality for duplicate detection in that form's body brackets.

**Impact:** Minor for `flat-list` (re-processing in transform pass), Minor for `full-expression` (additional branch in duplicate-check logic), Minor for `tokens` (one new AST variant).

### `src/ast.rs` — New Variants

```rust
Expr::RawTokens(Vec<Spanned<Token>>)  // for tokens receive mode
Expr::ParseStageMacroDecl { ... }     // for defparse-macro declarations
Expr::KeyIdentityDecl { ... }         // for declare-key-identity declarations
```

**Impact:** Minor — mechanical exhaustive match arm additions.

### `stdlib/syntax.llt` (new file)

```tinct
# Let-softening for fn, class, type
[defparse-macro fn [params: flat-list  body: expr]
  [if [let-decl? params]
    [list 'fn params body]
    [list 'fn [cons 'let params] body]]]

[defparse-macro class [tvars: flat-list  ...body: expr]
  [if [let-decl? tvars]
    [list 'class tvars ...body]
    [list 'class [cons 'let tvars] ...body]]]

[defparse-macro type [params: flat-list  body: expr]
  [if [let-decl? params]
    [list 'type params body]
    [list 'type [cons 'let params] body]]]

# Helpers
[wrap-in-let: [fn [let elems] [cons 'let elems]]]
[contains-structural-test?: [fn [let arm] ...]] # inspect arm for : entries
```

### `stdlib/prelude.llt` — New Parse-Stage Primitives

Add to the stdlib the inspection and construction primitives used by parse macros:
`let-decl?`, `var-ref?`, `annotated?`, `literal?`, `contains-structural-test?`, `wrap-in-let`, `make-let-decl`.

## Prerequisites

- **`[defmacro]`** (`doc/whatif/macros.md`) — parse-stage macros are a new class alongside post-parse macros; same infrastructure, different execution point
- **`[let ...]` as `Expr::LetDecl`** (`doc/whatif/unified-bindings.md`) — `let-decl?` must have something to detect; `Expr::LetDecl` must be a first-class AST node
- **Post-parse macro expansion** — the transformation pass builds on the existing expansion infrastructure

## References

- Tobin-Hochstadt, S. et al. (2011). "Languages as Libraries." *PLDI '11*, pp. 132–141. ACM. — [Racket's `#lang` mechanism; syntax classes scoped to macro bodies; tinct's approach is a targeted subset]
- Flatt, M. (2016). "Binding as sets of scopes." *POPL '16*, pp. 705–717. ACM. — [hygiene for macro-introduced bindings; the transformation pass must maintain hygiene]
- Flatt, M. & PLT (2010). "Reference: Racket." §Syntax Classes (`syntax-parse`) — [formal description of syntax classes and attribute binding; model for argument position declarations]
- Graham, P. (1993). "On Lisp." Prentice Hall. — [defmacro and quasiquote as the canonical macro body tools; the principle that macro bodies are ordinary code]
- Steele, G.L. (1990). "Common Lisp: The Language," 2nd ed. §7.2 "Macro Definitions." — [macros as functions from code to code; the transformation pass model]
- Pratt, V.R. (1973). "Top down operator precedence." *POPL '73*, pp. 41–51. ACM. — [Pratt parsing; relevant if infix operator extension is added later]
