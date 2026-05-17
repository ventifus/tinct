# What If: Parse-Stage Macros for tinct

**State:** Proposal

What would it take to let user-defined macros declare how their argument positions are parsed — controlling parse mode, key identity, and structural transformations — before arguments reach the main evaluator?

## Current State

Tinct's macro system (`doc/whatif/macros.md`) operates post-parse: macros receive fully-formed `Expr` AST dicts and return AST dicts. The parser always produces a complete AST before any macro runs, using fixed rules for every bracket it encounters:

- **All brackets are parsed as expressions** — implied call, dict, or type assertion based on content
- **Duplicate key detection uses bare-name identity** — `n@Int` and `n@String` are both field `"n"` → parse-time duplicate error
- **No argument transformations** — what the user writes is exactly what the macro receives; the macro cannot declare that a bracket should be wrapped in `[let ...]` or treated as a binding list

These rules are correct for general-purpose dicts. But they make it impossible for user-defined macros to use syntactic forms that require context-sensitive parsing — because the rejection or misinterpretation happens at parse time, before any macro runs.

### The Core Problem: Fixed Parse Rules Cannot Be Extended

Consider three distinct needs a user macro might have:

**1. Annotated-key dicts** (current proposal, narrow):
```tinct
[dispatch x
  n@Int:    i"int: $n"
  n@String: i"str: $n"   # PARSE ERROR — duplicate key "n"
  _:        "other"]
```
`n@Int` and `n@String` should be distinct keys. The parser rejects them as duplicates before any macro can run.

**2. Binding list positions** (let-softening use case):
```tinct
[my-fn [x y] body]   # user wants [x y] treated as a binding list
                     # equivalent to [my-fn [let x y] body]
```
The bracket `[x y]` should be wrapped in `[let ...]`, but only because of its argument position — not because the user wrote `[let ...]`.

**3. Pattern positions** (user-defined match-like forms):
```tinct
[my-match scrutinee
  [v: Ok]  → body1    # structural constructor pattern
  [n@Int]  → body2    # typed binding pattern
  _        → body3]   # wildcard
```
The arm keys need to be parsed as binding patterns (where `v: Ok` means structural test), not as dict entries (where `v: Ok` means key "v" with value `Ok`).

All three are impossible with post-parse macros because the parse-time semantics are fixed.

### What's Missing

1. **Argument position modes** — no way for a macro to declare that a specific argument position should be parsed differently (binding list, pattern, literal, etc.)
2. **Argument transformations** — no way for a macro to declare that a bracket should be wrapped or rewritten before it reaches the macro body
3. **Context-sensitive key identity** — no mechanism for a macro to declare that its body uses full-annotated-expression equality instead of bare-name equality for duplicate detection
4. **Variadic position declarations** — no way to declare how many arguments a form takes and what kind each is

## Why Parse-Stage Macros Matter for tinct

**User-defined language forms.** With parse-stage macros, a user can define `[my-for [let x] in collection body]`, `[my-match scrutinee arms...]`, or `[dispatch scrutinee arms-dict]` with the same syntactic clarity as built-in forms. Without them, user macros are second-class citizens whose argument positions are always parsed as generic expressions.

**The let-softening path.** The unified-bindings whatif (`doc/whatif/unified-bindings.md`) requires `[let ...]` uniformly at all binding positions. Parse-stage macros are the mechanism for a future softening: `[fn [x y] body]` → `[fn [let x y] body]` via a `binding-list` argument position declaration, making `[let ...]` optional where the context is unambiguous.

**Unforeseen uses.** A general argument position system covers cases not yet anticipated: custom DSLs embedded in tinct, syntax for testing frameworks, specialized configuration forms, domain-specific notations.

**Regular expressions and dicts unchanged.** Declarations only affect the specific macro forms they annotate. All existing parsing behavior is unchanged everywhere else.

## Design

Parse-stage macros are declared via `[declare-syntax name [arg-decl ...]]`. Each `arg-decl` describes one argument position: its mode, any transformation to apply, and optional validation.

### `[declare-syntax ...]`

```tinct
[declare-syntax form-name
  [pos1: mode1  transform1: t1  ...
   pos2: mode2  ...
   ...rest: modeN  ...]]
```

- **Positional** (`pos1:`, `pos2:`) — declare specific argument slots by index
- **Variadic** (`...rest:`) — declare the mode for all remaining arguments
- **Named** (`name:`) — declare how a keyword argument is parsed (for dict-body macros)

Declarations are registered during a **pre-parse pass** that processes `[declare-syntax ...]` forms at the top of a file (or at the top of a scope), before the main parse begins.

### Argument Position Modes

Each argument position has a **mode** that controls how its bracket is parsed:

| Mode | Description | Example input | What macro receives |
|------|-------------|---------------|---------------------|
| `expr` | Normal expression (default) | `[x y]` | `Call(x, [y])` or `Dict([x, y])` |
| `binding-list` | Binding list context | `[x@Int y]` | `LetDecl([Annotated(x, Int), VarRef(y)])` |
| `pattern` | Pattern context (structural tests enabled) | `[v: Ok]` | `LetDecl([StructuralBind(v, Ok)])` |
| `key-dict` | Dict with full-expression key identity | `[n@Int: body1  n@String: body2]` | Dict with distinct keys |
| `literal` | Must be a literal value | `"hello"` | `Str("hello")` |
| `name` | Must be a bare identifier | `foo` | `VarRef("foo")` |
| `raw` | Unevaluated bracket — sequence of tokens as a list | `[x y z]` | `List([Token(x), Token(y), Token(z)])` — for metaprogramming |

### Argument Position Transformations

In addition to mode, each position can have a **transformation** applied after parsing:

| Transformation | Description |
|----------------|-------------|
| `auto-let` | If the bracket doesn't start with `let`, wrap contents in `[let ...]`. Used with `binding-list` mode. |
| `auto-case` | If the first arg to `[case ...]` is not a `LetDecl`, wrap in `[let ...]`. Used with `pattern` mode. |
| _user-defined_ | A macro name that receives the parsed arg and returns a transformed `Expr`. Applies at parse stage before the main macro runs. |

### Worked Examples

**Key-identity for annotated-key dicts:**
```tinct
[declare-syntax dispatch
  [scrutinee: expr
   ...arms: key-dict]]

[dispatch result
  v: Ok    → [use v]
  e: Err   → [log e]
  n@Int:   i"int: $n"
  n@String: i"str: $n"   # valid — n@Int ≠ n@String under key-dict mode
  _:       "other"]
```

**Binding list position (let-softening):**
```tinct
[declare-syntax my-fn
  [params: binding-list  auto-let: true
   body: expr]]

[my-fn [x@Int y@Float] [+ x y]]
# → [my-fn [let x@Int y@Float] [+ x y]]  — [let ...] inserted automatically
```

**Pattern position (user-defined match-like form):**
```tinct
[declare-syntax my-match
  [scrutinee: expr
   ...arms: pattern-dict]]   # arms dict uses pattern-mode keys

[my-match result
  [let v: Ok]:  [use v]      # structural pattern — v binds Ok's payload
  [let n@Int]:  [+ n 1]      # typed binding
  [let _]:      0]            # wildcard
```

**Mixed positions:**
```tinct
[declare-syntax for-each
  [binding: binding-list  auto-let: true
   in: expr
   body: expr]]

[for-each [x@Int] in my-list [process x]]
# → [for-each [let x@Int] in my-list [process x]]
```

**Literal-only position:**
```tinct
[declare-syntax pragma
  [name: name      # must be a bare identifier
   value: literal]] # must be a literal

[pragma max-depth 256]   # ok
[pragma max-depth [+ 1 2]] # parse error: not a literal
```

**User-defined transformer:**
```tinct
[declare-syntax sql
  [query: raw   # raw token sequence
   transform: sql-tokenizer]]  # user-defined tokenizer macro

[sql SELECT * FROM users WHERE id = 42]
# sql-tokenizer receives the raw token list and produces an Expr
```

### Key Identity Modes for `key-dict`

When an argument position has `key-dict` mode, duplicate detection uses the **full expression node** as the key identity, not the extracted bare name:

| Key form | Bare-name identity (default) | Full-expression identity (`key-dict`) |
|----------|------------------------------|---------------------------------------|
| `n` | `"n"` | `VarRef("n")` |
| `n@Int` | `"n"` (collision!) | `Annotated("n", Simple("Int"))` |
| `n@String` | `"n"` (collision!) | `Annotated("n", Simple("String"))` |
| `_` | `"_"` | `VarRef("_")` |
| `42` | `42` | `Int(42)` |

Under `key-dict` mode, `n@Int` and `n@String` are structurally distinct — no collision. Two `n@Int` entries are still a duplicate. The macro receives a dict where the `key:` field of each entry contains the full expression node.

### What Macros Receive

The macro body receives the same AST dict shape as always — `[type: "dict" entries: [...]]` with each entry as `[type: "entry" key: ... value: ...]`. The parse-stage machinery changes what's IN those fields based on the declared mode, but the shape is uniform. Macros can dispatch on the `key:` field's type and content.

For `binding-list` mode, the macro receives an `Expr::LetDecl` where it would otherwise receive an `Expr::Dict`. For `key-dict` mode, the `key:` field contains the full annotated expression. For `raw` mode, the position contains a list of tokens rather than a parsed expression.

### Scope and Registration

`[declare-syntax ...]` forms are processed in a **pre-parse pass** that runs at the scope level before main parsing begins. Within a dict or file:

1. Scan for `[declare-syntax ...]` forms at the top level
2. Register their declarations in the syntax registry for the current scope
3. Parse all remaining forms with the registry active

Declarations in an inner dict scope override outer scope declarations for that form name. They do not affect sibling or parent scopes — following normal scoping rules.

### Security Model

Parse-stage macros do NOT get:
- **Lexer access** — no character-level tokenization hooks; the lexer is always Rust-only
- **Arbitrary code execution at parse time** — transformers receive already-parsed AST nodes and return AST nodes; no eval during parsing
- **Cross-file effects** — syntax declarations are scoped; they cannot affect other files or modules

The `raw` mode is the closest to lexer access, providing a list of tokens. But it only receives tokens within the specific argument position's bracket, and it returns an `Expr` — no ability to modify the surrounding parse.

## What Would Change

### `src/parser.rs` — Syntax Class Registry and Position Declarations

**Current:** Fixed duplicate-key detection using bare-name extraction. All brackets parsed as expressions.

**Proposed:** Before parsing `[name ...]`, check the syntax class registry for `name`. If a declaration exists:
- Apply the declared mode to each argument position
- Apply transformations (e.g., `auto-let`) after the position is parsed
- Use the declared key identity for `key-dict` positions

The registry is a `HashMap<String, SyntaxDecl>` populated during the pre-parse scan. `SyntaxDecl` stores:
```rust
struct SyntaxDecl {
    positional: Vec<ArgDecl>,       // indexed by position
    variadic: Option<ArgDecl>,      // for ...rest positions
    named: HashMap<String, ArgDecl>, // for keyword arguments
}

struct ArgDecl {
    mode: ParseMode,               // expr, binding-list, pattern, key-dict, literal, name, raw
    transform: Option<Transform>,  // auto-let, auto-case, or user-defined macro name
    key_identity: KeyIdentity,     // bare-name (default) or full-expression
}
```

**Impact:** Moderate. Pre-parse scan is O(n) over top-level forms. Registry lookup is O(1) per `[name ...]` form during main parse.

### `src/ast.rs` — `SyntaxDecl` in parsed output

**Proposed:** `Expr::SyntaxDecl` for the `[declare-syntax ...]` form itself (analogous to `Expr::TypeAlias`). Processed and consumed during the pre-parse pass; not evaluated at runtime.

**Impact:** Minor — one new AST variant.

### `src/expand.rs` — Pre-parse scan

**Proposed:** New `scan_syntax_decls(file: &Spanned<File>) -> SyntaxRegistry` function that processes `[declare-syntax ...]` forms before the main macro expansion pass. Returns a registry used by `parse2()` (or a post-parse transform) when resolving form names.

**Impact:** Moderate — new pass in the pipeline; must interoperate with the existing macro expansion order.

### `src/lexer.rs` — `raw` mode token lists

**Proposed:** When `raw` mode is declared for a position, the parser stores the raw `Vec<Spanned<Token>>` for that bracket instead of parsing it as an expression. A new `Expr::RawTokens(Vec<Spanned<Token>>)` variant holds this. User-defined transformer macros receive this and produce an `Expr`.

**Impact:** Minor — one new AST variant; conditional path in the parser bracket handler.

### `stdlib/syntax.llt` (new file)

Standard syntax declarations for common patterns:

```tinct
# Annotated-key dict
[syntax-class key-dict
  key-identity: full-expression]

# Binding list with auto-let
[syntax-class binding-list
  mode: let-decl
  auto-let: true]

# Pattern context
[syntax-class pattern
  mode: let-decl
  structural-tests: enabled]
```

### `doc/02-syntax.md` — `[declare-syntax ...]` documentation

Document the `[declare-syntax ...]` form, its argument position DSL, and the available modes and transformations.

## Prerequisites

- **`[defmacro]`** (`doc/whatif/macros.md`) — parse-stage macros extend, not replace. Fully implemented (`macro-integration` sprint, 2026-05-05).
- **`[let ...]` as a distinct parse form** (`doc/whatif/unified-bindings.md`) — `Expr::LetDecl` must be a first-class AST node for `binding-list` mode to produce it. Parse-stage macros provide the mechanism for later softening the `[let ...]` requirement where unambiguous.

## References

- Tobin-Hochstadt, S. et al. (2011). "Languages as Libraries." *PLDI '11*, pp. 132–141. ACM. — [Racket's `#lang` mechanism: language-level parse customization; tinct's syntax classes are a minimal, safe subset scoped to individual macro bodies rather than whole files]
- Flatt, M. (2016). "Binding as sets of scopes." *POPL '16*, pp. 705–717. ACM. — [hygiene for macro-introduced bindings; syntax class dispatch must maintain hygiene across parse-mode boundaries]
- Flatt, M. & PLT (2010). "Reference: Racket." §Syntax Classes (`syntax-parse`) — [formal description of syntax classes and attribute binding; the model for tinct's argument position declarations]
- Pratt, V.R. (1973). "Top down operator precedence." *POPL '73*, pp. 41–51. ACM. — [Pratt parsing for operator precedence; the algorithm for infix operator extension]
- Ford, B. (2002). "Packrat Parsing: Simple, Powerful, Lazy, Linear Time." *ICFP '02*, pp. 36–47. ACM. — [PEG parsing as an alternative for extensible grammars; context for why tinct's approach is a narrower, targeted extension]
- Krishnamurthi, S. (2001). "Linguistic Reuse." Ph.D. thesis, Rice University. — [syntactic abstraction and the role of parse-time hooks in language extensibility]
