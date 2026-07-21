# What If: Unified Binding Declarations via `[let ...]` for tinct

**State:** Completed — 2026-05-27

What would it take to give every binding declaration in tinct a single self-announcing form — so that binding brackets announce themselves rather than relying on context-specific parser special cases?

## Current State

Tinct binds names in several contexts, each with its own syntactic convention:

```tinct
[fn  [x@Integer y@Float]  body]           # param bracket — context makes it a binding list
[class  [a b c]  methods...]          # TypeVar bracket — context makes it a name list
[type  [a b]  [or a b]]               # alias param bracket — context makes it a name list
fn@[bind: [a b c]  return: c  ...]    # bind: value — treated as a name list
```

The same bracket form `[a b c]` means different things in different positions:

- In `[fn [a b c] body]` — three parameter names
- In `[class [a b c] ...]` — three TypeVar declarations
- In a value expression context — implied call `a(b, c)`

A reader unfamiliar with tinct sees `[fn [a b c] body]` and can reasonably parse `[a b c]` as calling function `a` with args `b` and `c`. The actual meaning — parameter declarations — is only knowable from the parser's context-specific special case for `fn`.

### What's Missing

1. A self-announcing binding form that is unambiguous without enclosing context
2. A consistent invariant: any bracket not starting with `let` is always an expression
3. Elimination of the per-keyword special cases that put brackets into "binding mode"

## Why Unified Bindings Matter for tinct

**One parsing invariant.** Today: `[a b c]` is an implied call, *except* when it appears as the first bracket after `fn`, `class`, or `type`. With `[let ...]`: `[a b c]` is always an implied call. No exceptions. The invariant is complete.

**Readable without foreknowledge.** `[fn [let x@Integer y@Float] body]` is unambiguous to a reader who has never seen tinct: the `let` keyword signals binding declaration. `[fn [x@Integer y@Float] body]` is not — it looks like calling `x@Integer` as a function with arg `y@Float`.

**Uniform match arms.** The `[case ...]` form makes match arm scoping explicit: `[case [let v] [Result.Ok v] body]` has `v` clearly declared in `[let v]` and scoped to `body` — the same mechanism as `[fn [let x] body]`. Current match arms (`[Ok v]: body`) look like dict entries; the scoping of `v` to `body` is implicit.

**Referential transparency of binding positions.** Because `[let ...]` is always a binding list and other brackets are always expressions, tools can identify all binding sites by checking for `let` as the first element — without knowing which enclosing keyword they're inside.

## Design

### The `[let ...]` Form

`let` is a keyword. `[let ...]` is a **binding declaration list** — always. Each element is a *binding pattern*:

- `name` — bare lowercase identifier, introduces an untyped binding
- `name@Type` — typed binding: name constrained to Type (type annotation, not structural test)
- `_` — wildcard, matches anything, introduces no binding

`[let ...]` **never parses as an implied call**. The `let` keyword is in the reserved keyword set.

Structural constructor tests (`name: Constructor`) are NOT part of the `[let ...]` element syntax. Instead, constructor testing and payload binding are expressed through the 3-argument `[case ...]` form: the `[let bindings]` declares which names are bound, and the separate `pattern` argument carries the constructor check. See the `[case ...]` section below.

### Function Parameters

```tinct
[fn [let x@Integer y@Float] [+ x y]]

[fn [let xs@[Seq a]  f@[Fn@b [a]]]
  [map f xs]]

# Zero params — empty [let]
[fn [let] 42]

# Variadic
[fn [let x@Integer  ...rest@[Seq Int]]
  [+ x [sum rest]]]
```

`[fn [x@Integer y@Float] body]` is a **parse error**: the parser expects `[let ...]` as the first bracketed expression inside `fn`.

### Class TypeVar Declarations

```tinct
Addable: [class [let a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [let a b]]]

Equatable: [class [let a]
  eq?: [fn@Boolean [let a a]]]
```

`[class [a b c] ...]` is a parse error.

### Type Alias Parameters

```tinct
Either:   [type [let a b] [or a b]]
Pair:     [type [let a b] [record first: a  second: b]]

# No-param alias — no [let ...], just the body type expression
NullableInt: [type [or Int Null]]
```

`[type [a b] body]` is a parse error for parameterized aliases.

### `bind:` in `fn@[...]`

The `bind:` key stays as the explicit metadata key. Its value uses `[let ...]`:

```tinct
scale: [fn@[bind: [let a b c]  return: c  constraint: [a: Numeric  b: Numeric  [$Multipliable a b c]]]
  [let x@a  factor@b]
  [* x factor]]
```

`bind: [a b c]` (without `let`) is a **type error** detected by the annotation resolver, not a parse error — `bind:`'s value is a dict value position and the parser cannot enforce keyword-specific constraints on dict values. The annotation resolver validates that the value of `bind:` is `Expr::LetDecl`; any other expression type produces "bind: value must be a [let ...] binding declaration."

### Instance Arm Keys

All type patterns in instance arms are binding lists. Instance arms use `[let ...]` directly:

```tinct
[instance Addable
  [let a@Integer   b@Integer   c@Integer  ]: [+: fn-int-add]
  [let a@Integer   b@Float c@Float]: [+: fn-int-float-add]]

[instance Appendable
  [let a@String]:        [concat: impl  empty: impl]
  [let a@[Seq elem]]: [concat: impl  empty: impl]
  [let a@[Map k v]]:  [concat: impl  empty: impl]]

[instance Functor
  [let f@Seq]:   [fmap: impl]
  [let f@Maybe]: [fmap: impl]]
```

### Match Arms — `[case ...]`

The scoping problem with current constructor patterns: `[Ok v]: body` uses `v` as both a pattern variable and a name that must scope to `body` — but the pattern system handles this implicitly, making the scoping relationship invisible. The `[case ...]` form makes scoping explicit by separating binding declarations from pattern structure:

```tinct
[case [let bindings]  pattern  body]
```

`[case ...]` takes **exactly 3 arguments**:

1. **`[let bindings]`** — declares which names in `pattern` are binding targets. Names listed here are introduced into `body`'s scope. An empty `[let]` means no new bindings.
2. **`pattern`** — the structural match expression. Dispatch depends on the head token:
   - Uppercase or dot-access head (`[Result.Ok v]`, `[Constructor field: v]`) → structural match against a nominal variant; names in `[let]` bind payload fields
   - Lowercase or operator head (`[> n 0]`, `[= n x]`) → guard expression, evaluated with all `[let]` names bound to the scrutinee
   - `_` → wildcard, always matches
3. **`body`** — evaluated when the arm matches, with `[let]` names in scope.

**Binding-name rule:** A name appearing in `pattern` that is listed in `[let bindings]` is a fresh binding. A name in `pattern` NOT listed in `[let]` is a pin — looked up from the enclosing scope and compared.

```tinct
[match result
  [case [let v]  [Result.Ok v]   v]           # bind v to Ok's payload
  [case [let e]  [Result.Err e]  [log e]]     # bind e to Err's payload
  [case [let _]  _               0]]          # wildcard

[match status
  [case [let]   200              "ok"]        # exact value — empty [let], no new bindings
  [case [let]   404              "missing"]
  [case [let n] n@Integer            [str n]]]    # typed binding

[match value
  [case [let n]  [> n 0]  "positive"]        # guard: lowercase head → guard expression
  [case [let n]  [< n 0]  "negative"]
  [case [let _]  _        "zero"]]           # wildcard fallback
```

### Exact-Value Matching

When the `pattern` argument to `[case ...]` is a literal or a name evaluated from the enclosing scope, the arm compares it to the scrutinee via `values_equal`. Use an empty `[let]` for arms with no new bindings:

```tinct
[case [let]  42         body]   # literal integer
[case [let]  "hello"    body]   # string literal
[case [let]  None       body]   # evaluates None from scope; undefined if not imported
[case [let]  sentinel   body]   # evaluates sentinel from scope; any casing valid
```

**Type restriction**: exact-value matching is valid only for scalar types (Int, Float, Bool, Str) and nullary constructor values. The type checker rejects exact-value arms where the expression's type is non-scalar and non-nullary (e.g., a constructor function type like `String → Result` — use `[case [let v] [Result.Ok v] body]` for non-nullary constructors).

**`values_equal` for nullary variants**: `values_equal` is extended to compare `Value::Variant { payload: None }` by tag equality. Two nullary variant values are equal iff their tags match.

**Soft-skip semantics**: if `values_equal` returns false, the arm is skipped. If `values_equal` raises (e.g., comparing incomparable types), it is a runtime error — not a skip.

**Exhaustiveness checking** — verifying that a `[match ...]` expression covers all constructors of the scrutinee's type — is out of scope for this proposal. All match expressions raise `MatchError` at runtime if no arm matches.

### Multi-Payload Constructor Representation

Multi-payload constructors pack their components into a single positional dict as the payload:

```tinct
[Pair 1 "hello"]   # runtime: Value::Variant { tag: "Pair", payload: Some(thunk([0: 1  1: "hello"])) }
[Triple a b c]     # runtime: Value::Variant { tag: "Triple", payload: Some(thunk([0: a  1: b  2: c])) }
```

Multi-payload destructuring uses the `pattern` argument of `[case ...]`: the bound names in `[let a b]` receive the positional dict payload fields — `a` binds to index 0, `b` to index 1. This reuses the existing positional dict pattern machinery.

Constructor declarations with multiple payloads specify the count:

```tinct
[type Point [Pt Float Float]]    # Pt has 2 payloads
[type Tree [Node Tree Tree]      # Node has 2 payloads (both Tree)
           [Leaf Int]]           # Leaf has 1 payload (Int)
```

### Constructor Payload Registry and Type Narrowing

The type checker builds a **constructor payload registry** from `[type ...]` nominal variant declarations. For each constructor, the registry records its payload type scheme (parameterized by the variant's type parameters):

```text
Registry entries from [type Result [Ok a] [Err String]]:
  Ok  → payload: a         (parameterized by Result's type param a)
  Err → payload: String    (concrete)
```

When typing `[case [let v] [Result.Ok v] body]` with scrutinee of type `Result String`:

1. Narrow the scrutinee type to the Ok branch: `scrutinee_ty ∩ Ok@String`
2. Look up Ok's payload type: `a` instantiated with `String` → `String`
3. In `body`: `v : String`, scrutinee narrowed to `Ok@String`

**Typing rules for `[let bindings]` elements:**

| `[let ...]` element | Type of introduced name | Narrowed scrutinee in body |
|---------------------|-------------------------|---------------------------|
| `[let n]` | `n : scrutinee_ty` | `scrutinee_ty` (unchanged) |
| `[let n@T]` | `n : scrutinee_ty ∩ T` | `scrutinee_ty ∩ T` |
| `[let _]` | (none) | `scrutinee_ty` |

Constructor narrowing is expressed through the `pattern` argument of `[case ...]`, not through `[let ...]` element syntax. When `pattern` has an uppercase/dot-access head (`[Result.Ok v]`), the type checker narrows the scrutinee to the constructor's tagged branch and types bound names from the constructor's payload type in the registry.

Constructor types are looked up from the local TypeEnv (scope-aware). An undefined constructor name in the pattern is a type error.

**`Unknown ∩ T` normalizes to `T`** (AGT, Garcia et al. 2016): intersection with the gradual type is identity. When the scrutinee type is `Unknown`, `[let n@Integer]` gives `n : Int` (not `n : Int & ?`). `normalize_intersection` must implement this case alongside the existing `Top`-as-identity rule.

**Unknown constructor warning** — if a constructor name in the `pattern` is looked up and not found in scope, it is a type error ("undefined variable: X"), same as any undefined name. If found in scope but not a constructor type, it is a type error ("X is not a constructor type"). There is no silent Unknown fallback for constructor names.

**Unknown payload**: if the constructor's payload type cannot be determined (scrutinee type is `Unknown`, or the constructor is not in the registry), payload bindings get type `Unknown`. This is sound under tinct's gradual typing model and does not prevent the arm from being used — it simply provides no static type information for the payload.

### `[let ...]` Validity

`[let ...]` produces `Expr::LetDecl` from any bracket starting with `let`. The parser is permissive — `Expr::LetDecl` can appear anywhere syntactically. The type checker enforces validity: `Expr::LetDecl` is only valid in these positions:

1. First expression inside `[fn ...]` (parameter list)
2. First expression inside `[class ...]` (TypeVar list)
3. First expression inside `[type ...]` when parameterized (alias params)
4. First expression inside `[case ...]` (binding name declarations)
5. First expression inside `[instance ...]` arms followed by `:` (arm key)
6. Value of `bind:` in `fn@[...]` metadata dict

Anywhere else (e.g., `[f [let x y]]` — a `LetDecl` as a function call argument), the type checker produces: "binding declaration `[let ...]` is not valid in expression position."

In fn params, only `name`, `name@Type`, `_`, and `...rest@Type` bindings are valid.

### Parsing Invariant

The parser has two complete rules for brackets:

> 1. A bracket starting with `let` is always a binding declaration list (`Expr::LetDecl`). Inside `[let ...]`, the implied-call rule is suspended. Binding patterns use `name`, `name@Type`, and `_`. The `:` token inside `[let ...]` is a parse error ("structural test syntax removed; use [case [let bindings] [Constructor v] body] form instead"). Nested `[...]` inside `[let ...]` is a binding-pattern group, not an expression.
> 2. Every other bracket is always an expression — implied call, positional dict, keyed dict, or type assertion — determined by content and context as today.

This is formally equivalent to having two syntactic sorts: *binding patterns* (introduced by `let`) and *expressions* (everything else). This is standard in multi-sorted abstract syntax (Harper, PFPL §1) and matches how ML and Haskell distinguish pattern positions from expression positions — the difference being that tinct's distinction is explicitly self-announced by the keyword rather than determined by enclosing context.

The context-sensitive rule within `[let ...]` — that `Token::OpenBracket` always pushes another `StackFrame::LetDecl` rather than running the content-based bracket classifier — is the one genuine parser change beyond adding a new keyword. This is explicit, bounded, and announced by the `let` keyword.

### The `_` Wildcard

`_` is currently a valid identifier in tinct. Inside `[let ...]`, `_` is the wildcard binding (no name introduced). Outside `[let ...]`, `_` continues to be a valid identifier. The distinction: inside `[let ...]`, `_` is recognized as a wildcard pattern; outside, it is an ordinary identifier reference. This is context-specific but contained within `[let ...]` scope — consistent with how `let` already changes bracket semantics.

### `...` — Placeholder Expression

`...` (three dots, no trailing identifier) is a first-class **placeholder expression** valid anywhere a value is expected. It produces a lazy thunk that raises `UnimplementedError` with its source span when materialized.

```tinct
# Abstract class method body — the canonical use
Equatable: [class [let a]
  eq?: [fn@Boolean [let a a] ...]]

# Stub function — type-checks, fails at call time
process: [fn@Result [let data@Input] ...]

# Partial expression — fails only when str forces the third arg
a: [fn [] [str "a" "b" ...]]

# Placeholder value in a config dict — fails only when accessed
config: [host: "localhost"  port: ...]

# Unreachable branch guard
[match x
  [case [let v]  [Result.Ok v]   v]
  [case [let _]  [Result.Err _]  ...]]   # should never execute

# Composes with laziness — no error until forced
x: ...
y: [+ x 1]         # thunk — no error yet
z: [y y y]         # thunk — no error yet
[emit [str z]]     # NOW fails: emit → str → z → y → x → ...
```

**Type checker:** `...` has type `Unknown` — the gradual escape hatch. It satisfies any type constraint without generating a type error. This makes `...` usable wherever a value of any type is needed.

**Evaluator:** `...` evaluates to `Thunk::new_placeholder(span)`. When any materialization path forces this thunk, it raises:

```text
UnimplementedError at <file>:<line>:<col>: ... placeholder reached
```

The source span points precisely to the `...` token. Tinct's existing materialization-span threading carries the call chain, so the full lazy evaluation path is visible in the error. **`UnimplementedError` is cacheable** — when a placeholder thunk is forced, the error is stored in `ThunkState::Failed` so subsequent forces return the memoized error without re-evaluation. **`UnimplementedError` is catchable** — `$try` intercepts it, returning `[Err unimplemented-error]`. This enables `...` as a first-class "required but unset" mechanism for dict values:

```tinct
config: [host: "localhost"  port: ...]   # port is required, not yet set

# get-or is a stdlib helper: use value if set, default if unimplemented
port: [$get-or config.port 8080]
```

`get-or: [fn [let val default] [match [$try val] [case [let v] [Result.Ok v] v] [case [let _] [Result.Err _] default]]]`

The error kind is `ErrorKind::Unimplemented` — callers can distinguish `...` placeholders from other errors (network failure, parse error, etc.) when needed.

**Disambiguation from other `...` uses:**

| Form | Context | Meaning |
|------|---------|---------|
| `...rest` | Param list | Variadic binding |
| `[name: Str ...]` | Type annotation | Open record rest |
| `...` | Value expression position (no following identifier) | Placeholder thunk |
| `...` | Inside `[let ...]` | Placeholder binding — a binding that raises UnimplementedError when the bound name is forced. Used in `[fn [let x@Integer ...] body]` to declare that some params are abstract/unimplemented. |

The third form is unambiguous at the token level: `Token::Spread` not followed by `Token::Identifier` in value position → `Expr::Placeholder`.

## What Would Change

### `src/lexer.rs` — `Token::Let` and `Token::Case`

**Current:** Neither `let` nor `case` exists as a keyword.  
**Proposed:** Add both: `Token::Let` and `Token::Case`. Neither identifier is available as a variable name.  
**Impact:** Minor — two new tokens; reserved word list grows by two.

### `src/ast.rs` — `Expr::LetDecl`, `Expr::CaseArm`, and `Expr::Placeholder`

**Current:** No `Expr::LetDecl`, `Expr::CaseArm`, or `Expr::Placeholder` variants.  
**Proposed:**

```rust
Expr::LetDecl {
    // Each element: one of:
    // - VarRef(name) — bare binding
    // - Annotated(name, ann) — typed binding (name@Type)
    // - Wildcard — _
    bindings: Vec<Spanned<Expr>>,
}

Expr::CaseArm {
    // 3-argument form: [case [let bindings] pattern body]
    // let_bindings: always Expr::LetDecl; declares which names are bound
    // pattern: the structural match expression (constructor, guard, wildcard, or literal)
    // body: evaluated when arm matches, with let_bindings names in scope
    let_bindings: Option<Arc<SurfaceNode>>,
    pattern: Arc<SurfaceNode>,
    body: Arc<SurfaceNode>,
}

Expr::Placeholder    // the ... expression; source span carried by Spanned<>
```

**Impact:** Minor — three new AST variants; exhaustive match arms gain new branches (mechanical).

### `src/parser.rs` — `StackFrame::LetDecl`, `StackFrame::CaseDecl`, frame updates

**Current:** fn/class/type frames use `parse_param_list()` (fn, defmacro) or context-specific first-bracket handling (class, type) to process binding brackets. Match frames use `pending_pattern_expr` with `expr_to_pattern_with_guard`.

**Proposed:**

**`StackFrame::LetDecl`**: Pushed when `[let` is encountered. Collects binding-pattern entries. Inside this frame, `Token::OpenBracket` unconditionally pushes another `StackFrame::LetDecl` (the one context-sensitive dispatch rule: the inner bracket is always a binding group, never an expression). Closes to `Expr::LetDecl`.

**`StackFrame::CaseDecl`**: Pushed when `[case` is encountered. Collects three expressions in order: first = `[let bindings]` (`Expr::LetDecl`), second = pattern, third = body. Closes to `Expr::CaseArm`. All three must be present; any missing argument is a parse error "case arm requires exactly 3 positional arguments: [let bindings] pattern body".

**Per-frame updates:**

- `StackFrame::Fn`: first expression must be `Expr::LetDecl`; parse error otherwise. (`parse_param_list()` is extended or replaced — the binding list now flows as `Expr::LetDecl` rather than being eagerly consumed as raw param entries.)
- `StackFrame::ClassDecl`: first expression must be `Expr::LetDecl`
- `StackFrame::TypeAlias`: if first expression is `Expr::LetDecl`, it is the param list; otherwise it is the body (no-param alias)
- `StackFrame::InstanceDecl`: `Expr::LetDecl` followed by `:` = arm key
- `StackFrame::Match`: `Expr::CaseArm` = 3-arg case arm; existing `pending_pattern_expr` path = shorthand keyed arms (`[Tag v]:`, `n@Integer:`, `...`:) which remain valid for non-binding arms

**`Expr::Placeholder` parsing**: `Token::Spread` not followed by `Token::Identifier` in value expression position → `Expr::Placeholder`. This is a content-based dispatch rule (same as other expression classifiers); no new StackFrame needed.

**`@` annotation context inside `StackFrame::LetDecl`**: when `Token::ImmediateAt` fires inside `StackFrame::LetDecl`, the following bracket is parsed as a type expression (annotation context), NOT as a sub-LetDecl. This is the existing `ImmediateAt` behavior — no change needed, but must be verified to take priority over the sub-LetDecl rule.

**`let:` and `case:` colon-ahead disambiguation**: `[let: value]` and `[case: value]` are valid dict key entries (not keyword forms) — the same colon-ahead rejection rule that applies to `fn`, `call`, and `type` keywords applies here: if the keyword identifier is immediately followed by `Token::Colon` (via `peek_next_horizontal`), it is dispatched as a dict key, not as a StackFrame keyword. This must be explicitly added to the existing colon-ahead check in the keyword dispatch table.

**Impact:** Moderate — two new StackFrame variants; three binding-bracket handler updates; one context-sensitive `OpenBracket` dispatch rule inside `StackFrame::LetDecl`; `Expr::Placeholder` requires no new StackFrame.

### `src/typecheck.rs` — binding extraction, case arm typing, and payload registry

**Current:** Binding extraction from raw bracket expressions, context-specific. No constructor payload registry lookup in pattern matching.

**Proposed:**

**Binding extraction**: each context (`fn`, `class`, `type`, `instance`, `case`) receives `Expr::LetDecl` and extracts bindings from `bindings: Vec<Spanned<Expr>>`. The semantic interpretation differs by context (value params vs TypeVars vs case bindings), but the extraction mechanics are shared.

**Case arm typing**: new `typecheck_case_arm(pattern: &Expr, scrutinee_ty: &Type, ...) -> (Environment, Type)` function that:

1. If pattern is `Expr::LetDecl`: processes each binding element against the scrutinee type per the typing rules table above
2. If pattern is a literal or nullary constructor expression: validates the type is scalar/nullary (type error otherwise), returns unmodified environment

**Type narrowing**: for `[let n@T]`, introduce `n` with type `is_subtype_intersect(scrutinee_ty, T)`. For `[let Constructor v]`, look up the constructor's payload type from the registry and introduce `v` with that type.

**Constructor payload registry**: populated during `[type ...]` processing. Each constructor entry stores its payload type scheme parameterized by the enclosing variant's type params. Queried during case arm typing to determine payload binding types.

**`[let ...]` validity check**: `Expr::LetDecl` outside binding positions → type error "binding declaration not valid in expression position."

**`Expr::Placeholder` typing**: `...` has type `Unknown` — the gradual escape hatch. Satisfies any type constraint without generating a type error. This mirrors `Any` in the value domain: it opts out of static checking for that position. The function body's inferred type is checked against the annotated return type via **consistency** (`~`), not strict subtyping (`<:`). This is why `...` (type `Unknown`) satisfies any annotated return type: `Unknown ~ Int` is true, but `Unknown <: Int` is false.

**Impact:** Major — new case arm typing path; constructor payload registry; type narrowing for pattern bindings; validity checking; `Expr::Placeholder` typed as `Unknown`.

### `src/eval.rs` — case arm evaluation

**Current:** Match arms use `match_pattern` + `expr_to_pattern_with_guard`; `values_equal` handles scalar equality.

**Proposed:**

**`eval_case_arm(pattern: &Expr, scrutinee: ThunkId, env: Rc<Environment>) -> Option<Rc<Environment>>`**:

- If `Expr::LetDecl`: call `eval_let_pattern(bindings, scrutinee, env)` which returns `Some(bound_env)` on success or `None` on structural mismatch (soft skip)
- If exact-value expression: evaluate the expression, call `values_equal`, return `Some(env)` on match or `None` on mismatch

**`eval_let_pattern`**: recursively processes binding elements:

- `VarRef(name)`: bind name to scrutinee thunk → `Some(env + [name: scrutinee])`
- `Annotated(name, Simple(TypeName))` where TypeName is lowercase: typed binding, check type, bind
- `Annotated(Constructor, bindings)` where Constructor is uppercase: materialize scrutinee, check tag matches Constructor, extract payload, recurse on payload with `bindings` → soft skip on tag mismatch
- Bracket group `[b₁ b₂ ...]`: materialize scrutinee as positional dict, bind bᵢ to index i-1 → soft skip if payload is not a positional dict of the right arity
- Wildcard `_`: always succeed, no binding

**Soft-skip rule**: any mismatch (tag mismatch, arity mismatch, type mismatch, `values_equal` returning false) returns `None` — the arm is skipped, not an error. If all arms return `None`, raise `MatchError`.

**`[let v: Ok]` against a unit variant**: `[let v: Ok]` against a scrutinee tagged `Ok` but with no payload (a unit variant) is a **soft skip** — the arm does not match. This is distinct from a type error: the pattern is well-formed, but this particular scrutinee value lacks the payload the pattern expects.

**Payload binding is strict**: when a structural test succeeds, the payload thunk is materialized before being bound. Payload bindings are always `Thunk::new_materialized(payload_value)`. This is a necessary strictness point — you must inspect the payload to destructure it.

**Evaluation order for nested patterns**: outside-in. The outer constructor test fires first; only if it succeeds is the payload extracted and the inner pattern applied. For `[let [[a b]: Pair]: Ok]`: Ok tag checked → payload extracted → Pair tag checked → payload extracted and destructured into a, b.

**`values_equal` extension**: add `Value::Variant { payload: None }` support (nullary constructors compare by tag only). Non-nullary variant comparison remains a type error, not a runtime mismatch.

**`Expr::Placeholder` evaluation**: `...` evaluates to `Thunk::new_placeholder(span)`. When this thunk is materialized by any path, it raises:

```text
UnimplementedError at <file>:<line>:<col>: ... placeholder reached
```

The source span (carried in `Spanned<Expr::Placeholder>`) is stored in the thunk and included in the error. Tinct's existing materialization-span threading shows the full lazy evaluation chain that led to the placeholder.

**`Expr::Placeholder` implementation**: Do NOT reuse `ThunkState::Placeholder` — it already serves as a letrec construction sentinel that `panic!()` when forced. Adding user-facing `...` semantics to it would destroy the ability to diagnose letrec bugs. Instead: in `eval_step`/`eval_recursive`, match `Expr::Placeholder` and return `Err(EvalError::unimplemented(span))` immediately. The laziness is preserved because `eval_step` only runs when a thunk is forced. The containing dict/fn/expression's thunk wraps the `Expr::Placeholder` as `Thunk::new_unevaluated`; it only errors when that thunk is forced.

**Impact:** Moderate — new `eval_case_arm` and `eval_let_pattern` functions; `values_equal` extended for nullary variants; `Thunk::new_placeholder` constructor added.

### `src/types.rs` — constructor payload registry

**Current:** Constructor types are registered in `TypeEnv` along with their type schemes from `[type ...]` declarations.
**Proposed:** No new data structure needed. When `[type Result [Ok a] [Err String]]` is processed, `Ok` is added to the **local TypeEnv** with type scheme `∀a. a → Result a`. When a case arm types `[let v: Ok]`, the type checker looks up `Ok` in the TypeEnv (scope-aware, follows normal scoping), reads the domain type of its function type scheme as the payload type. Two modules defining different constructors with the same name (`None`, `Ok`, etc.) have independent entries in their respective scoped TypeEnvs — no global constructor namespace exists.
**Impact:** No new HashMap or registry. Constructor payload types are derived from the existing TypeEnv during case arm typing. Scope-awareness is free — TypeEnv already follows dict scoping.

### `stdlib/prelude.llt` — binding syntax migration

**Current:** All fn/class/type/instance declarations use implicit binding brackets.  
**Proposed:** Migrate every binding bracket to `[let ...]`. Purely mechanical.  
**Impact:** Major in scope, minor in complexity.

### Corpus tests — binding syntax migration

**Current:** All test files use implicit binding brackets.  
**Proposed:** Migrate all `[fn [params]]`, `[class [tvars]]`, `[type [params]]` to `[let ...]` form.  
**Impact:** Moderate in scope, minor in complexity.

## Future: Parse-Stage Macro Softening

This proposal requires `[let ...]` uniformly at all binding positions. A future parse-stage macro system could introduce syntactic elision where it is safe and unambiguous — for example, `[fn [x@Integer y@Float] body]` expanding to `[fn [let x@Integer y@Float] body]` when the binding bracket contains no structural tests.

Note that `[case [let v] [Result.Ok v] body]` (3-arg structural match with explicit binding declaration) differs fundamentally from shorthand `[Result.Ok v]: body` (keyed arm) in that the binding scope is stated explicitly — any parse-stage elision for case arms must preserve the scoping semantics. Parse-stage macros (which operate before AST construction) are the right mechanism for this, as they can inspect token streams and apply context-sensitive transformations before the parser assigns structure.

The hard `[let ...]` requirement here is intentional and load-bearing — it establishes the clean semantic model that parse-stage macros will later soften in specific, well-defined ways.

## Prerequisites

None — this is a self-contained change. All semantic behaviors are defined within this proposal. No deferred items.

## References

- Milner, R. (1978). "A Theory of Type Polymorphism in Programming." *Journal of Computer and System Sciences*, 17(3), 348–375. — [`let` as the canonical polymorphic binding form in ML; the basis for explicit binding declarations]
- Landin, P.J. (1966). "The Next 700 Programming Languages." *Communications of the ACM*, 9(3), 157–166. — [ISWIM's `where`-clauses as syntactically distinct from application; the principle that binding and application should look different]
- Harper, R. (2016). *Practical Foundations for Programming Languages*, 2nd ed. Cambridge University Press, ch. 1. — [multi-sorted abstract syntax; binding occurrences as a distinct syntactic sort from expression occurrences; the formal grounding for why `[let ...]` is not "just another expression"]
- Amadio, R.M. & Cardelli, L. (1993). "Subtyping Recursive Types." *ACM TOPLAS*, 15(4). — [BAS intersection type narrowing used in pattern arm typing: `scrutinee_ty ∩ C` for narrowed arm bodies]
- Peyton Jones, S. (ed.) (2003). *Haskell 98 Language and Libraries: The Revised Report.* §3.17 — [case alternatives with `->` separator; constructor-first patterns; pattern-body scoping model]
