# What If: Unified Binding Declarations via `[let ...]` for tinct

**State:** Accepted — 2026-05-17

What would it take to give every binding declaration in tinct a single self-announcing form — so that binding brackets announce themselves rather than relying on context-specific parser special cases?

## Current State

Tinct binds names in several contexts, each with its own syntactic convention:

```tinct
[fn  [x@Int y@Float]  body]           # param bracket — context makes it a binding list
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

**Readable without foreknowledge.** `[fn [let x@Int y@Float] body]` is unambiguous to a reader who has never seen tinct: the `let` keyword signals binding declaration. `[fn [x@Int y@Float] body]` is not — it looks like calling `x@Int` as a function with arg `y@Float`.

**Uniform match arms.** The `[case ...]` form makes match arm scoping explicit: `[case [let v: Ok] body]` has `v` clearly scoped to `body` — the same mechanism as `[fn [let x] body]`. Current match arms (`[Ok v]: body`) look like dict entries; the scoping of `v` to `body` is implicit.

**Referential transparency of binding positions.** Because `[let ...]` is always a binding list and other brackets are always expressions, tools can identify all binding sites by checking for `let` as the first element — without knowing which enclosing keyword they're inside.

## Design

### The `[let ...]` Form

`let` is a keyword. `[let ...]` is a **binding declaration list** — always. Each element is a *binding pattern*:

- `name` — bare lowercase identifier, introduces an untyped binding
- `name@Type` — typed binding: name constrained to Type (type annotation, not structural test)
- `_` — wildcard, matches anything, introduces no binding
- `name: Constructor` — structural test: scrutinee must be this constructor; `name` binds to its payload. The `:` separates the binding name (left) from the structural constraint (right).
- `name@Type: Constructor` — typed single-payload structural test: test Constructor tag, payload must be Type, bind to name. Example: `[let v@Int: Ok]` means "test Ok tag, payload must be Int, bind to v".
- `[name₁ name₂ ...]: Constructor` — multi-payload structural test; bracket groups the payload bindings before `:`
- `_: Constructor` — structural test with no payload binding
- Nested: any binding pattern as the left-hand side of `:`

`[let ...]` **never parses as an implied call**. The `let` keyword is in the reserved keyword set.

The `:` separator reads as "from" or "extracted via": `v: Ok` = "v from Ok's payload." This is consistent with tinct's general `name: thing` pattern — the name is on the left, what it's associated with is on the right. The semantic distinction from dict entries: in `[let ...]`, `:` means structural extraction; outside `[let ...]`, `:` means dict key assignment. Both associate a name on the left with something on the right. The distinction is carried by the `[let ...]` binding context.

### Function Parameters

```tinct
[fn [let x@Int y@Float] [+ x y]]

[fn [let xs@Seq@a  f@[Fn@b [a]]]
  [map f xs]]

# Zero params — empty [let]
[fn [let] 42]

# Variadic
[fn [let x@Int  ...rest@Seq@Int]
  [+ x [sum rest]]]
```

`[fn [x@Int y@Float] body]` is a **parse error**: the parser expects `[let ...]` as the first bracketed expression inside `fn`.

### Class TypeVar Declarations

```tinct
Addable: [class [let a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [let a b]]]

Equatable: [class [let a]
  eq?: [fn@Bool [let a a]]]
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
  [let a@Int   b@Int   c@Int  ]: [+: fn-int-add]
  [let a@Int   b@Float c@Float]: [+: fn-int-float-add]]

[instance Appendable
  [let a@Str]:        [concat: impl  empty: impl]
  [let a@[Seq elem]]: [concat: impl  empty: impl]
  [let a@[Map k v]]:  [concat: impl  empty: impl]]

[instance Functor
  [let f@Seq]:   [fmap: impl]
  [let f@Maybe]: [fmap: impl]]
```

### Match Arms — `[case ...]`

The scoping problem with current constructor patterns: `[Ok v]: body` uses `v` as both a pattern variable and a name that must scope to `body` — but the pattern system handles this implicitly, making the scoping relationship invisible. The `[case ...]` form makes scoping explicit by putting `[let ...]` as the **first argument**, where its bindings naturally scope to everything that follows:

```tinct
[match scrutinee
  [case binding-pattern  body]
  [case binding-pattern  body]
  ...]
```

`binding-pattern` is either `[let ...]` (introduces new names) or a bare scalar/nullary expression (exact-value match, no new names):

```tinct
[match result
  [case [let v: Ok]   v]          # structural test Ok, bind payload to v
  [case [let e: Err]  [log e]]    # structural test Err, bind payload to e
  [case [let _]       0]]         # wildcard

[match status
  [case 200           "ok"]       # exact value — integer literal
  [case 404           "missing"]
  [case [let n@Int]   [str n]]]   # typed binding, no structural test
```

### Binding Patterns Inside `[let ...]` in `case`

**Plain binding**:
```tinct
[let n]       # bind n to scrutinee
[let n@Int]   # bind n, type constraint Int → n : scrutinee_ty ∩ Int
[let _]       # wildcard
```

**Single-payload structural test** — binding name first, `:`, constructor:
```tinct
[let v: Ok]      # test Ok tag, v binds to Ok's payload
[let v@Int: Ok]  # test Ok, payload must be Int, bind to v
[let _: Ok]      # test Ok, discard payload
```

`[let v: None]` where `None` is a nullary constructor (no payload) is a **type error** — use `[let _: None]` to test a nullary constructor.

The `@Type` annotation in `name@Type: Constructor` is a **compile-time type constraint** on the payload, not a runtime type test. The runtime only tests the constructor tag. In the arm body, `v : payload_type(Ok) ∩ Int`. If `payload_type(Ok) ∩ Int = Never` (e.g., Ok wraps a String), the type checker emits a dead-arm warning: "this arm can never match." The runtime never checks whether the payload is Int.

**Multi-payload structural test** — bracket groups the payload bindings before `:`:
```tinct
[let [a b]: Pair]              # test Pair, a gets first component, b gets second
[let [x@Float y@Float]: Pair]  # test Pair, both components must be Float
[let [a _ c]: Triple]          # test Triple, bind first and third, discard second
```

This example was originally used to argue that the `:` form reads better than juxtaposition — purely visual, not a special semantic case. The binding list `[let ok: Ok  a  b  c: Something  de: SomethingElse]` has five entries, which would match a five-component tuple scrutinee positionally (same rule as instance arms: N elements = N components, one-to-one). For a single-value scrutinee, five elements is an arity mismatch → type error.

**Nested structural patterns** — the left-hand side of `:` is itself a binding pattern:
```tinct
[let [[a b]: Pair]: Ok]    # test Ok; payload must be Pair; bind its components to a, b
[let [v: Ok]: Some]        # test Some; payload must be Ok; bind Ok's payload to v
[let [e: Err]: Some]       # test Some; payload must be Err; bind Err's payload to e
```

### Exact-Value Matching

When the first argument to `[case ...]` is NOT `[let ...]`, it is an exact-value match: the expression is evaluated from the enclosing scope and compared to the scrutinee via `values_equal`. This follows normal scoping rules — if the name is not in scope, it is an undefined variable error, not a silent fallback.

```tinct
[case 42         body]   # literal
[case "hello"    body]   # string literal
[case None       body]   # evaluates None from scope; undefined if not imported
[case sentinel   body]   # evaluates sentinel from scope; any casing valid
[case MyVar      body]   # evaluates MyVar from scope; uppercase variable names work
```

There is no case-convention disambiguation. Uppercase and lowercase names in exact-value position are treated identically — they are scope lookups. Whether a name refers to a constructor value or an ordinary variable depends only on what is in scope, not on its casing.

**Type restriction**: exact-value matching is valid only for scalar types (Int, Float, Bool, Str) and nullary constructor values. The type checker rejects exact-value arms where the expression's type is non-scalar and non-nullary (e.g., a constructor function type like `String → Result` — use `[let v: Ok]` for non-nullary constructors).

**`values_equal` for nullary variants**: `values_equal` is extended to compare `Value::Variant { payload: None }` by tag equality. Two nullary variant values are equal iff their tags match.

**Soft-skip semantics**: if `values_equal` returns false, the arm is skipped. If `values_equal` raises (e.g., comparing incomparable types), it is a runtime error — not a skip.

**Exhaustiveness checking** — verifying that a `[match ...]` expression covers all constructors of the scrutinee's type — is out of scope for this proposal. All match expressions raise `MatchError` at runtime if no arm matches.

### Multi-Payload Constructor Representation

Multi-payload constructors pack their components into a single positional dict as the payload:

```tinct
[Pair 1 "hello"]   # runtime: Value::Variant { tag: "Pair", payload: Some(thunk([0: 1  1: "hello"])) }
[Triple a b c]     # runtime: Value::Variant { tag: "Triple", payload: Some(thunk([0: a  1: b  2: c])) }
```

The bracket group `[a b]` in `[let [a b]: Pair]` destructures the positional dict payload: `a` binds to index 0, `b` binds to index 1. This reuses the existing positional dict pattern machinery.

Constructor declarations with multiple payloads specify the count:
```tinct
[type Point [Pt Float Float]]    # Pt has 2 payloads
[type Tree [Node Tree Tree]      # Node has 2 payloads (both Tree)
           [Leaf Int]]           # Leaf has 1 payload (Int)
```

### Constructor Payload Registry and Type Narrowing

The type checker builds a **constructor payload registry** from `[type ...]` nominal variant declarations. For each constructor, the registry records its payload type scheme (parameterized by the variant's type parameters):

```
Registry entries from [type Result [Ok a] [Err String]]:
  Ok  → payload: a         (parameterized by Result's type param a)
  Err → payload: String    (concrete)
```

When typing `[case [let v: Ok] body]` with scrutinee of type `Result String`:
1. Narrow the scrutinee type to the Ok branch: `scrutinee_ty ∩ Ok@String`
2. Look up Ok's payload type: `a` instantiated with `String` → `String`
3. In `body`: `v : String`, scrutinee narrowed to `Ok@String`

**Typing rules for binding patterns:**

| Pattern | Type of introduced names | Narrowed scrutinee in body |
|---------|--------------------------|---------------------------|
| `[let n]` | `n : scrutinee_ty` | `scrutinee_ty` (unchanged) |
| `[let n@T]` | `n : scrutinee_ty ∩ T` | `scrutinee_ty ∩ T` |
| `[let _]` | (none) | `scrutinee_ty` |
| `[let v: C]` | `v : domain(TypeEnv.lookup(C))` — the domain type of C's function type in scope | `scrutinee_ty ∩ C-tagged` |
| `[let [v₁ v₂]: C]` | `v₁ : component_1_type, v₂ : component_2_type` | `scrutinee_ty ∩ C-tagged` |
| `[let _: C]` / `[case C body]` | (none) | `scrutinee_ty ∩ C-tagged` |

Constructor types are looked up from the local TypeEnv (scope-aware). `[let v: Ok]` fails with "undefined variable: Ok" if Ok is not in scope.

**`Unknown ∩ T` normalizes to `T`** (AGT, Garcia et al. 2016): intersection with the gradual type is identity. When the scrutinee type is `Unknown`, `[let n@Int]` gives `n : Int` (not `n : Int & ?`). `normalize_intersection` must implement this case alongside the existing `Top`-as-identity rule.

**Multi-element `[let ...]` in case arms** — N elements in `[let ...]` match N components of the scrutinee positionally (same rule as instance arms: N elements = N class params, one-to-one). For a single-value scrutinee, only 1 element is valid. For a tuple scrutinee `[x y z]`, N elements match N components. Arity mismatch → type error. The type checker enforces that the element count equals the component count of the scrutinee's type.

**Arity mismatch in multi-payload destructuring** — `[let [a b c]: Pair]` when `Pair` has 2 registered components is a **type error** ("pattern has 3 bindings but Pair has 2 components"), not a silent `Unknown` fallback. The TypeEnv knows the arity of registered constructors.

**Unknown constructor warning** — if `Ok` is looked up and not found in scope, it is a type error ("undefined variable: Ok"), same as any undefined name. If found in scope but not a constructor type, it is a type error ("Ok is not a constructor type"). There is no silent Unknown fallback for constructor names.

**Nested pattern typing** is compositional — the narrowed scrutinee type flows into the inner pattern recursively:

```tinct
[case [let [[x y]: Pair]: Ok] body]
```
1. Narrow to `Ok` branch: get Ok payload type = `Pair Float Float` (from registry, instantiated)
2. Narrow payload to `Pair` branch: `x : Float`, `y : Float`
3. In `body`: `x : Float`, `y : Float`

**Unknown payload**: if the constructor's payload type cannot be determined (scrutinee type is `Unknown`, or the constructor is not in the registry), all payload bindings get type `Unknown`. This is sound under tinct's gradual typing model and does not prevent the arm from being used — it simply provides no static type information for the payload.

### `[let ...]` Validity

`[let ...]` produces `Expr::LetDecl` from any bracket starting with `let`. The parser is permissive — `Expr::LetDecl` can appear anywhere syntactically. The type checker enforces validity: `Expr::LetDecl` is only valid in these positions:

1. First expression inside `[fn ...]` (parameter list)
2. First expression inside `[class ...]` (TypeVar list)
3. First expression inside `[type ...]` when parameterized (alias params)
4. First expression inside `[case ...]` (binding pattern)
5. First expression inside `[instance ...]` arms followed by `:` (arm key)
6. Value of `bind:` in `fn@[...]` metadata dict

Anywhere else (e.g., `[f [let x y]]` — a `LetDecl` as a function call argument), the type checker produces: "binding declaration `[let ...]` is not valid in expression position."

- Structural test patterns (`name: Constructor`) are valid ONLY in case arm position (`[case ...]`). In fn parameter position, they are a type error: "structural test patterns are only valid in case arms, not function parameters." In fn params, only `name`, `name@Type`, `_`, and `...rest@Type` bindings are valid.

### Parsing Invariant

The parser has two complete rules for brackets:

> 1. A bracket starting with `let` is always a binding declaration list (`Expr::LetDecl`). Inside `[let ...]`, the implied-call rule is suspended. Binding patterns use `name`, `name@Type`, `_`, `name: Constructor`, `[names...]: Constructor`. The `:` token inside `[let ...]` is a structural-test separator, not a dict key separator. Nested `[...]` inside `[let ...]` is always a binding-pattern group, not an expression.
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
  eq?: [fn@Bool [let a a] ...]]

# Stub function — type-checks, fails at call time
process: [fn@Result [let data@Input] ...]

# Partial expression — fails only when str forces the third arg
a: [fn [] [str "a" "b" ...]]

# Placeholder value in a config dict — fails only when accessed
config: [host: "localhost"  port: ...]

# Unreachable branch guard
[match x
  [case [let v: Ok]  v]
  [case [let _: Err] ...]]   # should never execute

# Composes with laziness — no error until forced
x: ...
y: [+ x 1]         # thunk — no error yet
z: [y y y]         # thunk — no error yet
[emit [str z]]     # NOW fails: emit → str → z → y → x → ...
```

**Type checker:** `...` has type `Unknown` — the gradual escape hatch. It satisfies any type constraint without generating a type error. This makes `...` usable wherever a value of any type is needed.

**Evaluator:** `...` evaluates to `Thunk::new_placeholder(span)`. When any materialization path forces this thunk, it raises:
```
UnimplementedError at <file>:<line>:<col>: ... placeholder reached
```
The source span points precisely to the `...` token. Tinct's existing materialization-span threading carries the call chain, so the full lazy evaluation path is visible in the error. **`UnimplementedError` is cacheable** — when a placeholder thunk is forced, the error is stored in `ThunkState::Failed` so subsequent forces return the memoized error without re-evaluation. **`UnimplementedError` is catchable** — `$try` intercepts it, returning `[Err unimplemented-error]`. This enables `...` as a first-class "required but unset" mechanism for dict values:

```tinct
config: [host: "localhost"  port: ...]   # port is required, not yet set

# get-or is a stdlib helper: use value if set, default if unimplemented
port: [$get-or config.port 8080]
```

`get-or: [fn [let val default] [match [$try val] [case [let v: Ok] v] [case [let _: Err] default]]]`

The error kind is `ErrorKind::Unimplemented` — callers can distinguish `...` placeholders from other errors (network failure, parse error, etc.) when needed.

**Disambiguation from other `...` uses:**

| Form | Context | Meaning |
|------|---------|---------|
| `...rest` | Param list | Variadic binding |
| `[name: Str ...]` | Type annotation | Open record rest |
| `...` | Value expression position (no following identifier) | Placeholder thunk |
| `...` | Inside `[let ...]` | Placeholder binding — a binding that raises UnimplementedError when the bound name is forced. Used in `[fn [let x@Int ...] body]` to declare that some params are abstract/unimplemented. |

The third form is unambiguous at the token level: `Token::Spread` not followed by `Token::Identifier` in value position → `Expr::Placeholder`.

### Existing Match Shorthands

The current match arm shorthands (`[Ok v]:`, `n@Int:`, `_:`) remain valid. They coexist with `[case ...]` arms. Migration to `[case ...]` is encouraged but not required.

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
    // - Annotated(name, ann) — typed binding (name@Type) or structural test (name: Constructor)
    // - Wildcard — _
    // - LetDecl { .. } — nested bracket group for multi-payload
    bindings: Vec<Spanned<Expr>>,
}

Expr::CaseArm {
    // Either Expr::LetDecl (binding pattern) or any expression (exact-value match)
    pattern: Box<Spanned<Expr>>,
    body: Box<Spanned<Expr>>,
}

Expr::Placeholder    // the ... expression; source span carried by Spanned<>
```

**Impact:** Minor — three new AST variants; exhaustive match arms gain new branches (mechanical).

### `src/parser.rs` — `StackFrame::LetDecl`, `StackFrame::CaseDecl`, frame updates

**Current:** fn/class/type frames use `parse_param_list()` (fn, defmacro) or context-specific first-bracket handling (class, type) to process binding brackets. Match frames use `pending_pattern_expr` with `expr_to_pattern_with_guard`.

**Proposed:**

**`StackFrame::LetDecl`**: Pushed when `[let` is encountered. Collects binding-pattern entries. Inside this frame, `Token::OpenBracket` unconditionally pushes another `StackFrame::LetDecl` (the one context-sensitive dispatch rule: the inner bracket is always a binding group, never an expression). Closes to `Expr::LetDecl`.

**`StackFrame::CaseDecl`**: Pushed when `[case` is encountered. Collects two expressions: first = pattern (`Expr::LetDecl` or exact-value expression), second = body. Closes to `Expr::CaseArm`.

**Per-frame updates:**
- `StackFrame::Fn`: first expression must be `Expr::LetDecl`; parse error otherwise. (`parse_param_list()` is extended or replaced — the binding list now flows as `Expr::LetDecl` rather than being eagerly consumed as raw param entries.)
- `StackFrame::ClassDecl`: first expression must be `Expr::LetDecl`
- `StackFrame::TypeAlias`: if first expression is `Expr::LetDecl`, it is the param list; otherwise it is the body (no-param alias)
- `StackFrame::InstanceDecl`: `Expr::LetDecl` followed by `:` = arm key
- `StackFrame::Match`: `Expr::CaseArm` = new-style arm; existing `pending_pattern_expr` path = legacy shorthands (both coexist)

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
```
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

This proposal requires `[let ...]` uniformly at all binding positions. A future parse-stage macro system could introduce syntactic elision where it is safe and unambiguous — for example, `[fn [x@Int y@Float] body]` expanding to `[fn [let x@Int y@Float] body]` when the binding bracket contains no structural tests.

Note that `[case [v: Ok] body]` and `[case [let v: Ok] body]` are semantically distinct (the former is an exact-value dict match; the latter is a structural constructor match), so any parse-stage elision for case arms must be semantics-aware. Parse-stage macros (which operate before AST construction) are the right mechanism for this, as they can inspect token streams and apply context-sensitive transformations before the parser assigns meaning to `:`.

The hard `[let ...]` requirement here is intentional and load-bearing — it establishes the clean semantic model that parse-stage macros will later soften in specific, well-defined ways.

## Prerequisites

None — this is a self-contained change. All semantic behaviors are defined within this proposal. No deferred items.

## References

- Milner, R. (1978). "A Theory of Type Polymorphism in Programming." *Journal of Computer and System Sciences*, 17(3), 348–375. — [`let` as the canonical polymorphic binding form in ML; the basis for explicit binding declarations]
- Landin, P.J. (1966). "The Next 700 Programming Languages." *Communications of the ACM*, 9(3), 157–166. — [ISWIM's `where`-clauses as syntactically distinct from application; the principle that binding and application should look different]
- Harper, R. (2016). *Practical Foundations for Programming Languages*, 2nd ed. Cambridge University Press, ch. 1. — [multi-sorted abstract syntax; binding occurrences as a distinct syntactic sort from expression occurrences; the formal grounding for why `[let ...]` is not "just another expression"]
- Amadio, R.M. & Cardelli, L. (1993). "Subtyping Recursive Types." *ACM TOPLAS*, 15(4). — [BAS intersection type narrowing used in pattern arm typing: `scrutinee_ty ∩ C` for narrowed arm bodies]
- Peyton Jones, S. (ed.) (2003). *Haskell 98 Language and Libraries: The Revised Report.* §3.17 — [case alternatives with `->` separator; constructor-first patterns; pattern-body scoping model]
