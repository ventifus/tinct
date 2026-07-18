# Functions

## Explicit Function Application

**Implied call via bare identifier in head position.** When a bare identifier appears in head position, the bracket is a function call. The `call` keyword is optional — it remains valid for backwards compatibility and computed function expressions.

```tinct
["a" "b" "c"]          # Data — literal in head position
[f a b c]              # Function call — bare identifier "f" in head
[call f a b c]         # Explicit call (identical AST to implied)
```

Syntactically, `[f x]` is a bracket expression with unkeyed entries (the same parsing mechanism as `["a" "b" "c"]`). A bare identifier in head position triggers implied call: the parser interprets the expression as function + arguments. The AST represents this as a `Call` node with `func`, `args`, and `named_args` — not as a dict.

**Why:** Enables full lazy evaluation. Without `call`, the evaluator must eagerly materialize the head of every bracketed expression. With `call`, the entire application (including the function) can remain a thunk until materialized. The parser-level head-position rule preserves this property while making function calls more concise.

**Parser recognition:** The parser checks the first entry of every `[]`. If it matches a keyword (`call`, `fn`, `type`), the parser emits a specialized AST node. If it's a bare identifier (not followed by `:`), the parser emits a `Call` node (implied call). Otherwise it emits a `Dict` node. This is a parser-level decision, not an evaluator-level one.

```tinct
[f x y]                        # Parsed as CallExpr — implied call
[call f x y]                   # Parsed as CallExpr — explicit call (identical AST)
[fn [x] [+ x 1]]               # Parsed as FnExpr — function definition
```

**Edge cases:**

- `[call: something]` — the `:` makes `call` a key, not a keyword. Parsed as `Dict`.
- `[f]` — zero-argument call to `f` (Lisp-consistent: `(f)` is always application).
- `[$f]` — data: single-element sequence containing `ref(f)`. The `$` prefix prevents call interpretation.

**No built-in alias.** Users can define their own shorthand in stdlib or user code.

### Formal Grammar

These grammar rules are excerpts; see [Syntax](02-syntax.md) §Complete Grammar for the full definition including `keyword_call` and `keyword_fn`.

```ebnf
call_form = { keyword_call ~ value ~ call_args }

call_args = { (named_arg | value)* }

named_arg = { named_arg_key ~ ":" ~ value }

named_arg_key = @{ "$" ~ var_ident | bare_word }
```

**Note:** Both `$timeout: 60` and `timeout: 60` create a named argument with name `"timeout"`. The `$` prefix is syntactic sugar for readability (mirroring the escaped reference syntax) — the parser strips the `$` prefix, storing only `"timeout"` in the AST's `NamedArg.name` field. This ensures the argument name matches the parameter name directly during binding without prefix-stripping at evaluation time.

Arity enforcement uses per-parameter coverage, not a simple count — each required parameter (no `default:` annotation) must be covered by either a positional argument at its index or a named argument. Parameters with `default:` annotations are optional. This is enforced at evaluation time, not parse time — the parser recognizes `call` as a keyword (or implied call from bare identifier in head position) and emits a `Call` AST node, but arity checking beyond function-position detection is deferred to the evaluator (which has access to the function's parameter list). See [Call Convention — Formal Specification](#call-convention--formal-specification) for the formal C-COVERAGE, C-PRIORITY, C-NO-OVERLAP, and C-NAMED-VALID constraints.

Examples:

```tinct
[f x y]
[fetch "https://example.com" timeout: 60]
```

## Function Definition

**No `defn` special form.** Named functions are ordinary dict entries using `fn`. Parameters are always wrapped in a `[let ...]` binding declaration list:

```tinct
[
    double: [fn@Number [let x@Number] [* x 2]]
    add: [fn@Number [let x@Number y@Number] [+ x y]]
    
    # Full metadata dict form with constraint and doc
    min: [fn@[return: a  constraint: [a: Comparable]  doc: "Return smallest element"]
          [let xs@[Seq a]] ...]
]
```

**Why:** Consistent with dict-first design. Every binding is a key-value pair, no exceptions. Fewer special forms to implement.

**Function annotation forms:** `fn` supports two annotation forms:

- **Shorthand:** `fn@Type` — equivalent to `fn@[return: Type]`
- **Full metadata dict:** `fn@[return: ... constraint: ... doc: ...]` — all keys optional

See [Type Annotations](05-type-annotations.md) §fn@[...] Function Metadata Dict for constraint syntax and examples.

### Formal Grammar

```ebnf
fn_form = { keyword_fn ~ fn_annotation? ~ param_list ~ value+ }

fn_annotation = ${ "@" ~ annotation_value }

param_list = { "[" ~ (variadic_param | param)* ~ "]" }

param = ${ param_name ~ param_annotation? }

param_name = @{ (ASCII_ALPHA | "_") ~ (ASCII_ALPHA NUMERIC | "_" | "-")* ~ "?"? }

param_annotation = ${ "@" ~ annotation_value }

variadic_param = @{ "..." ~ param_name ~ param_annotation? }
```

Typed variadics (`...name@Type`) route each positional arg to the first bucket whose type matches (declaration order = match priority). An optional untyped fallback (`...rest`) catches any unmatched args and must appear last.

**Note:** The `value+` notation indicates that multiple body expressions are allowed. When multiple expressions are provided, they are wrapped in `Expr::Sequential` by the parser — intermediate expressions extend the function's environment, and the final expression is the return value. See `doc/08-evaluation.md` §Laziness Design for `Sequential` semantics.

Examples:

```tinct
[fn [x] x]
[fn@Number [x@Number y@Number] [+ x y]]
[fn@[return: Number  doc: "Sum"] [x@Number  y@[type: Number  default: 0]] [+ x y]]
[fn [f ...args] [map f args]]
[fn [f ...ns@Int ...rest] [head ns]]   # typed bucket + unannotated fallback
```

## Function Arguments

**Named args supported for any parameter (Kotlin model).**

```tinct
[fetch "https://example.com" timeout: 30  retries: 3]
```

## Variadic Parameters

**`...name` collects remaining arguments.** Consistent with `...` in type annotations for open records.

```tinct
->: [fn [data ...stages]
    [reduce [fn [acc f] [f acc]] data stages]]

# Called as:
[-> data step1 step2 step3]
# data = ..., stages = (cons-list of step1, step2, step3)
```

**Collection representation:** Variadic arguments use a **hybrid Seq/Dict representation** depending on annotation:

- **Annotated variadics** (`...xs@Seq` or `...xs@List`) → collected as a **Seq cons-list** for efficient lazy traversal
- **Unannotated variadics** (`...args` with no type annotation) → collected as a **Dict** with integer keys `{0: arg1, 1: arg2, ...}` and named arguments merged in with string keys

The Seq representation is preferred for performance (O(1) cons, lazy tail traversal). The Dict representation provides backward compatibility for unannotated variadics and supports mixed positional/named arguments in the same collection.

## Lambdas and `_` Shorthand

### No Auto-Curry

**`call` requires exact arity.** Passing too few or too many arguments is an error. Use lambdas or `_` shorthand to adapt arity.

```tinct
add: [fn@Number [x@Number y@Number] [+ x y]]

[add 1 2]                      # → 3 (exact arity)
[add 1]                        # ERROR: add expects 2 arguments, got 1
```

**`_` implicit lambda shorthand:** Any `[...]` expression that directly contains `_` (not nested inside an inner `[...]`) is automatically wrapped in a single-argument function. `_` becomes the parameter. All occurrences of `_` in that bracket refer to the same parameter.

```tinct
[add _ 1]                      # → [fn [_] [add _ 1]]
[> _.age 30]                   # → [fn [_] [> _.age 30]]
_.name                         # → [fn [_] _.name]  (access chain, no brackets)
```

**`_` in dict values:** `_` desugaring also applies to dict literals. If any entry value directly contains `_`, the entire dict is wrapped in an implicit lambda:

```tinct
[name: _.name  age: _.age]     # → [fn [_] [name: _.name  age: _.age]]
```

This is useful for creating projection functions in pipelines:

```tinct
[map [name: _.name  age: _.age] users]
```

**`_` in func position:** `_` in the function position of `[_ ...]` does **not** trigger implicit lambda desugaring. Only `_` in arguments, named arguments, dict values, and access chains triggers desugaring. `[_ x]` is a call where the function is looked up from the variable `_`, not an implicit lambda.

**`_` in dict entry keys:** `_` as a dict entry key (e.g., `[_: value]`) does **not** trigger desugaring. Only `_` as the *target* of a dot access chain (e.g., `_.name`) or as a direct argument triggers implicit lambda wrapping.

**Scoping rule:** The lambda boundary is the innermost `[...]` that directly contains `_`. Nested bracket expressions that contain their own `_` create separate lambdas:

```tinct
[filter [> _.age 30] users]
#       └─ inner _ ─┘
# Inner [> _.age 30] contains _ → becomes [fn [_] [> _.age 30]]
# Outer [filter ...] does NOT contain _ directly → stays as-is
# Result: [filter [fn [_] [> _.age 30]] users]
```

**Pipeline interaction:** `->` threads a value through a list of single-argument functions. Each pipeline step is either a function reference (for 1-arg functions) or a `_` expression that creates an implicit lambda:

```tinct
[-> data.users
    [filter [> _.age 30] _]    # two _ levels: inner = element, outer = collection
    [map _.name _]             # inner _.name = element transform, outer _ = collection
    sort]                      # ref: already 1-arg
```

Desugaring of `[filter [> _.age 30] _]`:

1. Inner `[> _.age 30]` contains `_` → `[fn [_] [> _.age 30]]`
2. Outer `[filter ... _]` still contains `_` → `[fn [_] [filter [fn [_] [> _.age 30]] _]]`
3. Each `_` binds to its innermost enclosing lambda (lexical scoping)

**`apply` spreads a list into function arguments:**

```tinct
args: [5 10]
[apply + args]                 # → [+ 5 10] → 15
```

**Why not auto-curry:** Auto-currying makes arity errors silent. Pass too few arguments and you get a partial application instead of an error. Explicit arity checking catches mistakes.

### `_` Desugaring — Formal Specification

`_` desugaring is a **pre-typecheck source-to-source AST transformation**. It runs after parsing and before both type checking and evaluation. The type checker and evaluator both see the desugared form (Scala, Clojure, and Elixir all desugar placeholder syntax before evaluation — none gate on the runtime environment). See Pombrio & Krishnamurthi (2014) for the formal framework motivating pre-evaluation desugaring; Krishnamurthi (2012, PLAI) for the standard pipeline ordering.

**Pipeline placement:**

```text
source → parse → desugar → resolve → typecheck → eval
```

The pass operates on `SurfaceProgram` (multi-document). All entry points call `desugar_surface_program()` before resolve and before eval. The type checker and evaluator receive the already-desugared program.

**DIRECT predicate.** Tests whether an expression is `_` or a dot access chain rooted at `_`. Operates on **raw** (pre-desugaring) AST nodes. Dict entry keys are excluded — only the access *target* triggers desugaring:

```text
DIRECT(e) = match e with:
  | VarRef("_")              → true
  | DotAccess(e', _)         → DIRECT(e')
  | Pipe(e', _)              → DIRECT(e')   -- enables WRAP-PIPE on chained pipes: $_ | f | g
  | _                        → false
```

Note: `BracketAccess` and `RangeAccess` AST variants do not exist in this language — only dot access chains and pipes are supported as DIRECT extensions. The Pipe case allows `$_ | f | g` to be recognized as DIRECT at the outermost level, so WRAP-PIPE fires correctly on chained pipe expressions.

**Rewrite rules.** The pass checks WRAP conditions on **raw** (un-desugared) children *before* recursing. DIRECT subtrees are left as-is inside the generated `Fn` body — they are variable references to the `_` parameter, not candidates for further wrapping. Non-DIRECT children are recursed into at depth+1 (inside the generated lambda, `_` is bound). This avoids the greedy-wrapping problem where naive bottom-up traversal would wrap `_.age` before its enclosing Call could claim it (Visser 1998).

```text
DESUGAR(e, depth) =
  -- Fn with _ param: increase depth, recurse into body only
  | Fn(params, body) where "_" ∈ params
      → Fn(params, DESUGAR(body, depth + 1))

  -- At depth > 0, _ is bound — recurse children, never wrap
  | _ where depth > 0
      → RECURSE_CHILDREN(e, depth)

  -- WRAP-CALL: check DIRECT on args/named values, then wrap
  | Call(f, args, named)
      where (∃ a ∈ args. DIRECT(a)
             or ∃ n ∈ named. DIRECT(n.value))
      → Fn([_], Call(                                    -- [WRAP-CALL]
            DESUGAR(f, depth + 1),                       -- recurse func
            [DESUGAR(a, depth + 1) | a ∈ args],          -- recurse all args
            [n{value=DESUGAR(n.value, depth + 1)}        -- recurse all named vals
             | n ∈ named]))

  -- WRAP-DICT: same pattern — check raw, wrap, recurse all values
  | Dict(entries)
      where ∃ entry ∈ entries. DIRECT(entry.value)
      → Fn([_], Dict(                                   -- [WRAP-DICT]
            [e{value=DESUGAR(e.value, depth + 1)}
             | e ∈ entries]))

  -- WRAP-DOT: standalone access chain rooted at _
  -- Only fires when no enclosing Call/Dict claimed it
  | DotAccess(target, field)
      where DIRECT(target)
      → Fn([_], DotAccess(target, field))                -- [WRAP-DOT]

  -- WRAP-PIPE: pipe with DIRECT lhs wraps the whole pipe chain
  -- Pipe lowering (Pipe → Call) runs after wrapping, inside RECURSE_CHILDREN
  | Pipe(lhs, rhs)
      where DIRECT(lhs)
      → Fn([_], Pipe(                                    -- [WRAP-PIPE]
            DESUGAR(lhs, depth + 1),
            DESUGAR(rhs, depth + 1)))

  -- Note: There are no WRAP-BRACKET or WRAP-RANGE rules.
  -- BracketAccess and RangeAccess AST variants do not exist in this language.

  -- PASS: no wrapping, recurse into all children
  | _ → RECURSE_CHILDREN(e, depth)                       -- [PASS]
```

**WRAP rules summary:**

| Rule | Condition | Result |
|------|-----------|--------|
| WRAP-CALL | Any arg or named arg value is DIRECT (func position excluded) | Wrap in `Fn([_], Call(...))` |
| WRAP-DICT | Any entry value is DIRECT | Wrap in `Fn([_], Dict(...))` |
| WRAP-DOT | Standalone `_.field` (not inside a Call/Dict that claims it) | Wrap in `Fn([_], DotAccess(...))` |
| WRAP-PIPE | `$_ \| f` — lhs is `$_` (implicit arg) | Wrap in `Fn([_], Pipe(_, f))` |

Note: There are no WRAP-BRACKET or WRAP-RANGE rules — bracket and range access are not part of the language.

**Exclusions.** The following positions do **not** trigger desugaring:

- **Func position in Call:** The function position is excluded from the DIRECT check (not from the wrapping). WRAP-CALL fires when any arg or named value is DIRECT, regardless of whether the function itself is also DIRECT. When both func and an arg are DIRECT (e.g., `[_ _]`), wrapping produces `[fn [_] [_ _]]` where both references bind to the same `_` parameter. A bare `[_ x]` (func is DIRECT, no args are DIRECT) does not trigger WRAP-CALL; the func `_` falls through to PASS and is recursed normally.
- **Dict entry keys:** `[_: value]` — WRAP-DICT checks `DIRECT(entry.value)` only, never `entry.key`.
- **TypeAssert values:** `[@Number _.age]` — TypeAssert is not a WRAP form. The inner `_.age` triggers WRAP-DOT independently, producing `[@Number [fn [_] _.age]]` (a type assertion on a function). This is likely a user error; the type checker will report a mismatch.

**Boundary forms and scoping.** `Dict`, `Call`, and `Fn` are **lambda boundaries**. The WRAP rules check raw children before recursing, so each `_` binds to the innermost enclosing bracket that triggers a WRAP rule:

```text
[filter [> _.age 30] users]

Traversal (top-down check, selective recursion):
  1. Outer Call: DIRECT(users)? No. DIRECT([> _.age 30])? No (Call is
     not DIRECT). No WRAP. RECURSE_CHILDREN.
  2. Inner Call: DIRECT(_.age)? Yes (in args). WRAP-CALL fires.
     → Fn([_], [> _.age 30])
  3. Outer Call now has args = [<fn>, users] — neither is DIRECT. Unchanged.
  Result: [filter [fn [_] [> _.age 30]] users]  ✓
```

**Shadowing.** If `_` is a parameter of an enclosing `Fn`, inner `_` references refer to that parameter — they are ordinary variable references, not desugaring triggers. The `depth` parameter tracks this lexically:

- `depth = 0`: `_` is unbound, WRAP rules apply.
- `depth > 0`: `_` is bound by an enclosing `Fn([_] ...)`, RECURSE_CHILDREN only.

This replaced the eval-time `env.borrow().get("_").is_none()` check with a purely syntactic scope analysis. The lexical approach is more precise: desugaring depends only on AST structure, never on the runtime environment.

**Invariants:**

1. **Syntactic determinism.** The desugaring result depends only on the AST structure, never on the runtime environment. The same expression always desugars the same way.
2. **Idempotence.** Applying `DESUGAR` to an already-desugared AST produces no changes (the generated `Fn` nodes have `_` as a single parameter, setting depth > 0 for inner references).
3. **Type visibility.** After desugaring, the type checker sees `Fn` nodes and can infer function types for `_` expressions. With the current type checker (unannotated params default to `Type::Any`), `[add _ 1]` types as `Fn(Any → Number)`. With future bidirectional checking, the call-site context could refine the parameter type — e.g., `[map _.name users]` where `users: Seq[[name: Str ...]]` could check the lambda against `Fn([name: Str ...] → Str)`. Row-polymorphic parameter inference (see row-unification section) would further improve this to `Fn([name: α ...ρ] → α)`.

**Span preservation.** Generated `Fn` nodes reuse the span of the original expression. Error messages reference user-written syntax (`[add _ 1]`), not the desugared form (`[fn [_] [add _ 1]]`).

**Implementation sketch:**

```rust
fn desugar_surface_program(program: &mut SurfaceProgram) {
    for doc in &mut program.documents {
        for item in &mut doc.node.items {
            if let SurfaceItem::Expr(node) = item {
                desugar_surface_node(node, 0);
            }
        }
    }
}

// Public entry point — stable API surface; delegates to private impl.
pub fn desugar_surface_node(node: &mut Arc<SurfaceNode>, depth: usize) {
    desugar_surface(node, depth);
}

// Private recursive implementation.
fn desugar_surface(node: &mut Arc<SurfaceNode>, depth: usize) {
    // Check WRAP conditions on raw children BEFORE recursing
    if depth == 0 && try_wrap_surface(node) {
        // Recurse into the generated lambda body at depth+1
        if let SurfaceExpression::Fn { body, .. } = &mut Arc::make_mut(node).expr {
            desugar_surface(body, 1);
        }
        return;
    }
    // At depth > 0 or no WRAP match: recurse into children
    recurse_children_surface(node, depth);
}
```

**Implementation location.** The desugaring pass lives in `src/desugar.rs`. The entry point is `desugar_surface_program()` for multi-document programs. Unit tests for `_` desugaring call `desugar_surface_program()` before `surface_program_to_file()` and `eval()`.

#### Testing Requirements

Corpus tests are required for each WRAP rule (WRAP-CALL, WRAP-DICT, WRAP-DOT, WRAP-PIPE) and each exclusion position (func position in Call, dict entry keys). Tests should verify that desugaring produces the expected `Fn([_], ...)` wrapper and that excluded positions do not trigger wrapping.

## Call Convention — Formal Specification

Specifies how arguments at a call site are bound to function parameters. This is a dual-layer specification: **binding constraints** (declarative — what a valid binding is) and a **binding algorithm** (phased operational rules — how to compute it), connected by a **correctness proof** showing the algorithm computes the unique solution satisfying the constraints.

The constraint layer draws on Garrigue's (1995) treatment of labeled and optional arguments, which separates the binding environment for default evaluation from the closure environment. The phased algorithm follows the Kotlin/Scala model: any parameter is nameable at the call site, required and optional parameters may be freely interleaved in declarations, and the arity constraint is a per-parameter coverage check rather than a simple count (see C-COVERAGE below).

### Notation

A function definition `[fn [p₁ p₂@[default: e₂] ...p₃] body]` has:

| Symbol | Meaning |
|--------|---------|
| `P = [p₁, ..., pₙ]` | Regular (non-variadic) parameters, ordered by position |
| `V` | Variadic parameter (if present): the `...name` param, always last |
| `required(pᵢ)` | `true` iff pᵢ has no `default:` annotation |
| `default(pᵢ)` | The default expression from pᵢ's `default:` annotation |
| `R = \|{pᵢ ∈ P \| required(pᵢ)}\|` | Count of required parameters |

A call site `[call $f a₁ a₂ k₁: v₁]` provides:

| Symbol | Meaning |
|--------|---------|
| `pos = [θ₁, ..., θₘ]` | Positional argument thunks, in order |
| `named = {k₁↦θ'₁, ..., kⱼ↦θ'ⱼ}` | Named argument thunks, keyed by name |
| `env_d` | Environment for evaluating default expressions |
| `env_c` | Closure environment (parent of the call environment) |

The environment parameter `env_d` is caller-controlled (Garrigue 1995): for normal calls, `env_d` is the caller's environment; for `$apply`, `env_d` is the closure environment (since `$apply` has no caller-side AST context for defaults).

### Part 1: Binding Constraints (Declarative)

A **valid binding** for parameters `P`, optional variadic `V`, positional args `pos`, named args `named`, and default environment `env_d` is an environment `env_call` (with parent `env_c`) satisfying all of the following constraints simultaneously:

**[C-COVERAGE] Per-parameter coverage (Kotlin model):**

```text
∀pᵢ ∈ P where required(pᵢ):  i < |pos|  ∨  pᵢ.name ∈ dom(named)
V = ∅ ⟹ |pos| ≤ |P|                         (no excess args without variadic)
```

Every required parameter must be covered by either a positional argument at its index or a named argument. This replaces a simple count-based arity check (`|pos| ≥ R`), which is insufficient when required parameters are interleaved with optional ones. Example: `[fn [a@[default: 1] b] body]` with one positional arg — count-based check passes (1 ≥ 1) but `b` at index 1 is unreachable.

**[C-PRIORITY] Binding priority chain:**

For each pᵢ ∈ P, exactly one case applies (in priority order):

```text
(i)   i < |pos|                               ⟹  env_call(pᵢ) = pos[i]
(ii)  i ≥ |pos| ∧ pᵢ.name ∈ dom(named)       ⟹  env_call(pᵢ) = named[pᵢ.name]
(iii) i ≥ |pos| ∧ pᵢ.name ∉ dom(named)
      ∧ ¬required(pᵢ)                         ⟹  env_call(pᵢ) = eval(default(pᵢ), env_d)
```

If none of the three cases applies (i.e., i ≥ |pos|, not named, and required), C-COVERAGE is violated — no valid binding exists.

**[C-NO-OVERLAP] Positional/named exclusivity:**

```text
∀(k, _) ∈ named:  ¬∃i < |pos| such that pᵢ.name = k
```

A named argument must not target a parameter already bound positionally.

**[C-NAMED-VALID] Named argument validity:**

```text
∀(k, _) ∈ named:  (∃pᵢ ∈ P such that pᵢ.name = k)  ∨  V ≠ ∅
```

Named arguments may target any parameter (required or optional), but must target an existing parameter OR the function must have a variadic parameter. This enables the Kotlin model: to reach a required parameter past an optional one, name it at the call site. When a variadic parameter exists, unmatched named arguments flow into the variadic collection alongside excess positional arguments.

**[C-VARIADIC] Variadic collection:**

```text
unmatched_named = {k↦θ ∈ named | ¬∃pᵢ ∈ P such that pᵢ.name = k}
V ≠ ∅ ⟹ env_call(V) = Dict({Int(k)↦pos[|P|+k] | k ∈ 0..(|pos|-|P|)}
                             ∪ {Str(k)↦θ | (k,θ) ∈ unmatched_named})
```

Excess positional arguments (beyond `|P|`) are collected into a Dict with integer keys starting at 0. Unmatched named arguments (those not targeting any parameter in P) are merged into the same Dict with string keys. If `|pos| = |P|` and `unmatched_named = ∅`, the variadic Dict is empty (`{}`).

**[C-COMPLETE] Completeness:**

```text
∀pᵢ ∈ P:  pᵢ.name ∈ dom(env_call)
V ≠ ∅ ⟹ V.name ∈ dom(env_call)
```

Every parameter receives a binding.

### Part 2: Binding Algorithm (Phased Rules)

Five sequential phases compute the binding. The output of each phase flows into the next. The judgment form is `bind(P, V, pos, named, env_d, env_c) ⇒ env_call | error`.

**[BIND-SPLIT]**

```text
params = [p₁, ..., pₙ]
    pₙ.variadic = true  →  P = [p₁, ..., pₙ₋₁],  V = pₙ
    otherwise            →  P = [p₁, ..., pₙ],     V = ∅
───────────────────────────
split(params) ⇒ (P, V)
```

The variadic parameter, if present, is always the last parameter. This is enforced by the parser.

**[BIND-ARITY]**

```text
For each pᵢ ∈ P where required(pᵢ):
    if i ≥ |pos| ∧ pᵢ.name ∉ dom(named):
        error("missing argument for required parameter '{pᵢ.name}'")

V = ∅ ∧ |pos| > |P|         ⟹  error("arity mismatch: expected at most |P| arguments, got |pos|")
otherwise                    ⟹  pass
───────────────────────────
arity_check(P, V, pos, named) ⇒ pass | error
```

Per-parameter coverage check: each required parameter must be reachable via positional index or named argument. This handles interleaved required/optional parameters correctly — a required param at index 3 with an optional param at index 2 is valid if the required param is provided by name.

**[BIND-POSITIONAL]**

```text
env₀ = Environment(parent: env_c)
For i = 0, ..., |P|-1:
    if i < |pos|:
        envᵢ₊₁ = envᵢ[pᵢ.name ↦ pos[i]]                          (positional arg)
    else if pᵢ.name ∈ dom(named):
        envᵢ₊₁ = envᵢ[pᵢ.name ↦ named[pᵢ.name]]                  (named arg fills gap)
    else if ¬required(pᵢ):
        envᵢ₊₁ = envᵢ[pᵢ.name ↦ eval(default(pᵢ), env_d, d+1)]  (default value)
    else:
        unreachable (BIND-ARITY guarantees every required pᵢ has i < |pos| ∨ pᵢ.name ∈ dom(named))
───────────────────────────
bind_positional(P, pos, named, env_d, env_c) ⇒ env_{|P|}
```

Parameters are bound left-to-right. For each parameter, the priority chain determines the source: positional arg first, then named arg, then default. This phase consumes named args that fill gaps beyond the positional args — BIND-NAMED handles only the unconsumed remainder.

The `env_d` parameter controls where default expressions are evaluated — this is the Garrigue (1995) separation. Defaults are wrapped as lazy thunks; errors surface at the parameter's first use in the body, not at the call site.

**[BIND-NAMED]** (validation and unmatched collection)

```text
unmatched_named = ∅
For each (k, θ) ∈ named:
    if ∃i < |pos| such that pᵢ.name = k:
        error("parameter 'k' received both positional and named argument")
    if ¬∃pᵢ ∈ P such that pᵢ.name = k:
        if V = ∅:
            error("unexpected named argument: \"k\" (valid parameter names: p₀.name, …, p_{n-1}.name)")
        else:
            unmatched_named = unmatched_named ∪ {k↦θ}
───────────────────────────
bind_named(P, V, pos, named, env_{|P|}) ⇒ (env_{|P|}, unmatched_named) | error
```

BIND-NAMED was originally a pure validation phase, but now also collects unmatched named arguments for variadic functions. All named args that target valid parameters were already consumed by BIND-POSITIONAL (which checks `pᵢ.name ∈ dom(named)` for params past the positional args). After BIND-POSITIONAL, every param in P is bound in `env_{|P|}`. BIND-NAMED verifies two conditions: (1) overlap — no named arg targets a positionally-bound parameter, (2) existence (amended for B-277) — every named arg must target an existing parameter OR the function must have a variadic parameter (in which case the unmatched arg is collected for merging into the variadic Dict). Named args may target any parameter (required or optional) — this is the Kotlin model.

The implementation may split this into two loops for engineering clarity (one for overlap, one for existence/collection) without affecting semantics.

**[BIND-VARIADIC]**

```text
V ≠ ∅:
    var_dict = Dict({Int(k)↦pos[|P|+k] | k ∈ 0..(|pos|-|P|)}
                     ∪ {Str(k)↦θ | (k,θ) ∈ unmatched_named})
    env_call = env'[V.name ↦ Materialized(var_dict)]
V = ∅:
    env_call = env'
───────────────────────────
bind_variadic(V, P, pos, unmatched_named, env') ⇒ env_call
```

The variadic parameter receives a Dict with integer keys (for excess positional args) and string keys (for unmatched named args). The Dict is materialized immediately (not a thunk) — the values within it are thunks from the positional args and named args, preserving laziness of the individual arguments.

### Part 3: Correctness Proof

**Theorem (Correctness of Binding Algorithm).** The phased binding algorithm (Part 2) computes the unique valid binding (Part 1) when one exists, and produces an error otherwise.

The proof has three parts: uniqueness of the declarative solution, soundness of the algorithm, and completeness.

**Uniqueness.** For each pᵢ ∈ P, the priority chain [C-PRIORITY] is deterministic: cases (i), (ii), (iii) are mutually exclusive because they partition the space by the condition `i < |pos|` and membership `pᵢ.name ∈ dom(named)`. Given fixed inputs, at most one case applies per parameter, so at most one environment satisfies all constraints simultaneously. The variadic binding [C-VARIADIC] is likewise deterministic (a fixed subsequence of `pos`). ∎

**Soundness.** Assume the algorithm produces `env_call` without error. Show each constraint holds:

- **C-COVERAGE:** BIND-ARITY explicitly checks per-parameter coverage for each required param and the upper bound. If the algorithm proceeds past BIND-ARITY, both conditions hold. ✓

- **C-PRIORITY:** BIND-POSITIONAL iterates over P in order. For each pᵢ:
  - If `i < |pos|`: binds `pos[i]` — matches case (i).
  - If `i ≥ |pos|` and `pᵢ.name ∈ dom(named)`: binds `named[pᵢ.name]` — matches case (ii).
  - If `i ≥ |pos|` and `pᵢ.name ∉ dom(named)` and `¬required(pᵢ)`: binds default — matches case (iii).
  - The else branch is unreachable: BIND-ARITY guarantees every required pᵢ has `i < |pos| ∨ pᵢ.name ∈ dom(named)`, so at least one of cases (i) or (ii) applies.

  Each case in the algorithm corresponds exactly to the matching constraint case. ✓

- **C-NO-OVERLAP:** BIND-NAMED checks `∃i < |pos| such that pᵢ.name = k` for each named arg and errors if true. If no error, the constraint holds. ✓

- **C-NAMED-VALID:** BIND-NAMED checks that each named arg targets an existing parameter. If no error, the constraint holds. ✓

- **C-VARIADIC:** BIND-VARIADIC constructs exactly the Dict specified by the constraint. ✓

- **C-COMPLETE:** BIND-POSITIONAL binds every pᵢ ∈ P (loop runs for all |P| params). BIND-VARIADIC binds V if present. ✓

All constraints satisfied. ∎

**Completeness.** Assume the constraints have a valid solution. Show the algorithm does not error:

- BIND-ARITY: C-COVERAGE guarantees every required pᵢ has `i < |pos| ∨ pᵢ.name ∈ dom(named)`, and `V = ∅ ⟹ |pos| ≤ |P|`. All checks pass.
- BIND-POSITIONAL: For each pᵢ where `i ≥ |pos|`: either `pᵢ.name ∈ dom(named)` (case ii of C-PRIORITY) or `¬required(pᵢ)` (case iii). The else branch is unreachable.
- BIND-NAMED overlap check: C-NO-OVERLAP guarantees no named arg targets a positionally-bound param.
- BIND-NAMED existence check: C-NAMED-VALID guarantees all named args target existing params.
- BIND-VARIADIC: No error conditions.

No error is produced. ∎

**Corollary (Unique binding).** Since at most one valid binding exists (Uniqueness) and the algorithm produces a binding whenever one exists (Completeness + Soundness), `bind_args_thunks` produces exactly the unique valid binding — or an error if no valid binding exists.

### Part 4: Error Taxonomy

The binding algorithm produces four distinct error classes. Each corresponds to a constraint violation:

| Error | Constraint violated | Message pattern | Source |
|-------|-------------------|-----------------|--------|
| Uncovered required param | C-COVERAGE | `"missing argument for required parameter '{pᵢ.name}'"` | BIND-ARITY |
| Too many args | C-COVERAGE (upper) | `"arity mismatch: expected at most {|P|} arguments, got {|pos|}"` | BIND-ARITY |
| Positional/named overlap | C-NO-OVERLAP | `"parameter '{k}' received both positional and named argument"` | BIND-NAMED |
| Nonexistent named arg | C-NAMED-VALID | `"unexpected named argument: \"{k}\" (valid parameter names: {p₀, …, p_{n-1}})"` | BIND-NAMED |

Default evaluation errors (from `eval(default(pᵢ), env_d)` in BIND-POSITIONAL) are not binding errors — they propagate as normal evaluation errors with the default expression's span.

**Implementation note:** The evaluator uses per-parameter coverage (C-COVERAGE) and accepts named args for any parameter, not just `default:` params.

### Part 5: `$apply` and the Default Environment

**Dict-splitting.** `$apply` takes a function `f` and a single dict argument `D`. Before invoking the binding algorithm, it splits `D` into positional and named argument lists:

```text
pos   = sort_by_key({ (k, v) ∈ D | k ∈ Int })    # integer-keyed entries, sorted by key
named = { (k, v) ∈ D | k ∈ String }              # string-keyed entries, as named args
```

Integer-keyed entries become positional arguments (in ascending key order); string-keyed entries become named arguments. The resulting `pos` and `named` are passed to `bind_args_thunks` exactly as if the caller had written them inline.

The `default_env` parameter is the key difference between normal calls and `$apply`:

```text
eval_call:     default_env = caller's environment (env)
$apply:        default_env = closure environment  (env_c)
```

**Why `$apply` uses `env_c`:** `$apply` receives a function value and a dict of arguments at runtime — there is no caller-side AST context. Default expressions reference names from the function's definition site, not the call site. Using the closure environment ensures defaults resolve correctly.

**Formal consequence:** The binding constraints [C-PRIORITY case (iii)] use `eval(default(pᵢ), env_d)`. The environment `env_d` is a parameter of the judgment, not fixed. This makes the specification parametric over the default evaluation strategy — both `eval_call` and `$apply` are instances of the same binding algorithm with different `env_d` values.

**Correctness is preserved:** The correctness proof (Part 3) is parametric in `env_d`. Changing `env_d` affects which values defaults evaluate to, but not the structure of the binding (which params get positional vs named vs default). Soundness, completeness, and uniqueness hold for any `env_d`.

**Variadic typing precision:** The type checker assigns variadic parameters type `Record([], Closed)` regardless of actual arguments (§Type Inference Algorithm, Limitation #8). The runtime Dict has integer-keyed entries with the excess args' types. A precise type would require dependent types (the length depends on `|pos| - |P|`). The current typing is a sound over-approximation — accessing variadic fields produces type errors that succeed at runtime. See Limitation #8 for the correct type (`Record([], Open)` or `Any`).

**PendingCall interaction:** When a `PendingCall` thunk is materialized, it invokes `invoke_function`, which calls `bind_args_thunks` — the same binding algorithm specified above. The materialization semantics (state transitions, memoization, error handling) are specified in §Thunk Lifecycle — Formal Specification, rules MATERIALIZE-CALL and MATERIALIZE-CALL-BUILTIN.

### Part 6: Worked Example

Trace all five phases for a call with interleaved required/optional parameters:

```tinct
greet: [fn [greeting@[default: "hello"] name sep@[default: " "]]
    [str greeting sep name]]

[greet name: "Alice"]
```

**BIND-SPLIT:** `params = [greeting, name, sep]`. No variadic.

- `P = [greeting, name, sep]`, `V = ∅`

**BIND-ARITY:** Required params = `{name (index 1)}`.

- `name`: `1 < |pos|`? No (`|pos| = 0`). `"name" ∈ dom(named)`? Yes. ✓ Covered.
- Upper bound: `|pos| = 0 ≤ |P| = 3`. ✓

**BIND-POSITIONAL:** `pos = []`, `named = {"name"↦θ_Alice}`.

| i | param | `i < \|pos\|`? | `name ∈ dom(named)`? | `¬required`? | Binding |
|---|-------|-----------|------------------|------------|---------|
| 0 | `greeting` | No (0 < 0) | No | Yes | `eval("hello", env_d)` → `"hello"` |
| 1 | `name` | No (1 < 0) | Yes | — | `named["name"]` → `θ_Alice` |
| 2 | `sep` | No (2 < 0) | No | Yes | `eval(" ", env_d)` → `" "` |

Result: `env₃ = {greeting↦"hello", name↦θ_Alice, sep↦" "}`

**BIND-NAMED:** Validate named args.

- `("name", θ_Alice)`: overlap? `∃i < 0` with `pᵢ.name = "name"`? No. ✓ Exists? `name ∈ P`? Yes. ✓

**BIND-VARIADIC:** `V = ∅`, skip.

**Result:** `env_call = {greeting↦"hello", name↦θ_Alice, sep↦" "}`. Evaluates to `"hello Alice"`.

Without the Kotlin model, this call would fail — `name` has no `default:`, so it couldn't be named. The caller would have to write `[greet "hello" "Alice"]`, defeating the purpose of `greeting`'s default.

**`PendingCall` thunk state:**

To make dict-returning operations lazy, the thunk model gains a new state:

`PendingCall` represents "apply this function to these arguments when materialized." It enables lazy function application at runtime without constructing AST nodes. When a `PendingCall` thunk is materialized, it calls the function and memoizes the result (transitioning to `Materialized`), just like `PendingBuiltin` does for builtin calls. The full field set (including `named` args, `caller_env`, and `EvalContext`) is specified in [Evaluation](08-evaluation.md) §Thunk Lifecycle, [MATERIALIZE-CALL].

This is different from `PendingBuiltin` in a key way:

- **PendingBuiltin** stores a Rust function pointer (`BuiltinFn`) and its arguments — the builtin runs when materialized
- **PendingCall** stores a user-defined function thunk, its argument thunks, and a `call_span: Span` (for error reporting) — invokes `invoke_function()` when materialized

Both support lazy evaluation, but `PendingCall` works at the Tinct function level (no AST needed), while `PendingBuiltin` works at the Rust builtin level.

**Type transparency:** `PendingCall` is invisible to the type system — a `PendingCall(f, [x])` has the same inferred type as `f(x)`. No new `Type` variant is needed; HM type inference is unchanged.

**Error reporting:** When `PendingCall` materialization fails, the definition-site span comes from the function's body, the materialization-site span from where the thunk was materialized, and a stack frame is added with the deferred call's creation span (from `call_span`).

**Motivation:** Operations like `map` on dicts need to create new thunks that apply a function to each value, but they can't store AST nodes (the function comes from a runtime variable). `PendingCall` lets them defer function application without needing to construct new AST `CallExpr` nodes.
