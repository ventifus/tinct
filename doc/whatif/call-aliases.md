# What If: Custom Call Aliases for tinct

What would it take to let users define aliases or alternatives for the
`call` keyword?

## Current State

tinct uses explicit `[call $f $x]` for all function application. The
`call` keyword is mandatory — there is no implicit head evaluation.
This is a core design principle (doc/01-introduction.md §Principle 3: Explicit
Function Application):

```lisp
[call $map [fn [x] [call $* $x 2]] $data]
```

Users sometimes want shorter syntax for frequent function calls, e.g.,
`[apply $f $x]` or `[$f $x]` (implicit call).

Current workarounds include function wrappers and existing ergonomic
forms:

```lisp
# Function wrappers — note: spread-in-call ([call $f ...args]) is not valid
# tinct syntax; $apply is a builtin for this purpose
pipe:  [fn [x f] [call $f $x]]

# $_ implicit lambda
[call $map [call $double $_] $data]

# $-> threading eliminates nested calls
[call $-> $input $parse $transform $format]
```

### What's Missing

1. User-defined syntax forms that look like `call` but carry
   additional semantics (logging, timing, retry).
2. Domain-specific application vocabulary — a DSL author cannot make
   `[invoke ...]` or `[run ...]` mean "call with custom behavior."
3. Syntactic extensibility at the bracket level — wrappers like
   `apply` above still require `[call $apply ...]`, adding a layer
   rather than replacing one.

## What Call Aliases Would Provide

1. **Domain-specific dispatch.** A DSL author could define
   `[invoke ...]`, `[run ...]`, or `[query ...]` as semantically
   meaningful application forms in specific contexts.

2. **Augmented application semantics.** A macro-defined call form
   could add logging, timing, or error wrapping around function calls
   without requiring the user to remember a wrapper function name.

3. **Syntactic parity with Lisp tradition.** Lisp macros have provided
   user-defined special forms for 60 years. tinct's bracket syntax is
   well-suited to the same extensibility mechanism.

## Design

Custom call forms are implemented as procedural AST macros, not as
parser-level aliases. The `call` keyword remains the only built-in
application form; macros expand to `call` at compile time.

### Macro-Based Call Forms

When the macro system is adopted (`doc/whatif/macros.md`), users
define custom call forms as macros:

```lisp
# Define a macro that adds timing to function calls
[defmacro timed [f ...args]
  [quote
    [let [start: [call $now]]
      result: [call [unquote $f] [unquote-splice $args]]
      elapsed: [call $- [call $now] $start]
      [call $log [call $str "Elapsed: " $elapsed "ms"]]
      $result]]]

# Usage
[timed $process $data]  # expands to call + timing wrapper
```

### Why Macros, Not Parser Aliases

Principle 3 is load-bearing: `call` is what makes tinct's bracket
syntax unambiguous. Every `[call ...]` is a function call; every
other `[...]` is data (a dict or list). Parser-level aliases would
break this invariant — the parser would need to know which
identifiers are call aliases to determine whether a bracket expression
is a call or data. This couples parsing to the environment, which
PEG parsers (Ford, 2004) cannot express (PEGs are context-free over
the input string).

Macros preserve the invariant differently: macro expansion happens
after parsing, on the AST. The parser sees `[timed $process $data]`
as a bracket expression (data). The macro expander recognizes `timed`
as a macro name and rewrites the AST to `[call ...]` form before
type checking and evaluation. This is exactly how Lisp macros work —
the reader produces S-expressions, the macro expander rewrites them,
and the evaluator sees only core forms.

### Scope and Hygiene

Macro definitions are scoped to the defining module. A macro defined
in `dsl/timing.llt` does not leak into other files unless explicitly
imported. This prevents the "action at a distance" problem where an
imported library changes the meaning of bracket syntax.

Hygienic macros (Kohlbecker et al., 1986) ensure that variable names
introduced by macro expansion do not capture user variables. The
`start`, `result`, and `elapsed` bindings in the `timed` example
above should be gensymed to avoid shadowing user-defined variables
with the same names.

### Interaction with Type Inference

Macro expansion happens before type inference. The type checker sees
only the expanded `[call ...]` forms, so no changes to Algorithm W
or the unification engine are needed. This is the standard approach:
macros are syntactic sugar, not type-level constructs. Typed macros
(as in Typed Racket) are a different design point that tinct does not
need for this use case.

### Interaction with Lazy Evaluation

Because macros expand to standard `call` expressions, they inherit
tinct's lazy evaluation semantics. Each macro-expanded argument is a
thunk, forced on demand. The `timed` example above relies on this:
`$now` calls must be forced eagerly (the timing wrapper explicitly
forces them via `let` bindings), but the user's function arguments
follow normal lazy semantics.

### Interaction with Error Reporting

Macro-expanded code may produce confusing error messages if errors
reference the expanded form rather than the source form. Source
location tracking through macro expansion (mapping expanded AST
nodes back to the original macro invocation site) is important for
usability. This is a known challenge — Rust's `proc_macro::Span`
and Racket's syntax objects both address it with source location
propagation.

## What Would Change

### Parser (src/parser.rs)

**Current:** The parser recognizes `call` as the only application
keyword. Bracket expressions without `call` are parsed as data
(dicts/lists).
**Proposed:** No change to the parser. Macro forms parse as regular
bracket expressions (data). Macro expansion is a separate pass.
**Impact:** None — this is the key advantage of the macro approach.

### AST / Macro Expander (new: src/macros.rs)

**Current:** No macro system exists.
**Proposed:** New macro expansion pass between parsing and type
checking. The expander walks the AST, recognizes macro invocations,
and rewrites them to core forms. Requires a macro definition
registry and quasiquoting support.
**Impact:** Major — new compiler phase. However, this is the same
impact as adopting macros in general (`doc/whatif/macros.md`); call
aliases add no additional complexity beyond the macro system itself.

### Type Checker (src/typecheck.rs)

**Current:** Type-checks `call` expressions.
**Proposed:** No change — macro expansion produces standard `call`
expressions before type checking begins.
**Impact:** None.

### Evaluator (src/eval.rs)

**Current:** Evaluates `call` expressions.
**Proposed:** No change — macro expansion produces standard `call`
expressions before evaluation begins.
**Impact:** None.

### Formatter (src/formatter.rs)

**Current:** Formats `[call ...]` expressions.
**Proposed:** Preserve macro invocation syntax in formatted output.
`[timed $f $x]` should not be expanded to `[call ...]` by the
formatter. This requires the formatter to operate on pre-expansion
AST (which it already does, since it operates on the token stream).
**Impact:** Minor — no change needed if the formatter continues to
operate on the token stream.

## Phased Adoption

### Phase 1: Macro System Foundation

Implement procedural AST macros (`doc/whatif/macros.md`). This
provides the infrastructure for any syntactic extension, including
custom call forms. The macro system is the prerequisite — call
aliases are a use case, not a separate feature.

### Phase 2: Standard Call Macros

Ship example macros in `stdlib/macros/` that demonstrate custom call
patterns:

- `[timed $f $args...]` — call with timing
- `[traced $f $args...]` — call with argument/result logging
- `[retry $n $f $args...]` — call with retry on failure

These serve as documentation and templates for users writing their
own call forms.

### Prerequisites

- Macro system (`doc/whatif/macros.md`) — specifically the `defmacro`
  form and AST expansion pipeline.
- Quasiquoting (`doc/whatif/quasiquoting.md`) — for ergonomic macro
  definitions. Without quasiquoting, macro bodies require verbose
  manual AST construction.

### Trigger

- When the macro system is implemented.
- When users request domain-specific call forms with custom semantics
  (logging, retry, tracing).
- When a DSL built on tinct needs application syntax beyond `call`.

## References

- doc/01-introduction.md §Principle 3: Explicit Function Application — "`call` is
  not syntactic overhead — it's what makes tinct's bracket syntax
  unambiguous."
- doc/whatif/macros.md — Procedural AST macros proposal.
- doc/whatif/quasiquoting.md — Quasiquoting for ergonomic macro
  definitions.
- Ford, B. (2004). "Parsing expression grammars: a recognition-based
  syntactic foundation." *POPL '04*, pp. 111–122. — PEGs are
  context-free over the input; parser-level call aliases would require
  context-sensitivity that PEGs cannot express.
- Kohlbecker, E., Friedman, D.P., Felleisen, M., & Duba, B. (1986).
  "Hygienic macro expansion." *LFP '86*, pp. 151–161. — Foundation
  for macro hygiene. Macro-introduced bindings must not capture user
  variables.
- Clinger, W. & Rees, J. (1991). "Macros that work." *POPL '91*,
  pp. 155–162. — Extends hygienic macros to handle macro-defining
  macros and referential transparency.
- Flatt, M. (2002). "Composable and compilable macros: you want it
  when?" *ICFP '02*, pp. 72–83. — Racket's phase-separated macro
  system. Relevant if tinct macros need to import helper functions
  at expansion time.
