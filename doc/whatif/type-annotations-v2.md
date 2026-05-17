# What If: Type Annotations V2 for tinct

**State:** Accepted — 2026-05-14

What would it take to make type annotations in tinct composable, extensible, and grounded in the type-stage evaluator — so that annotation brackets are evaluated the same way as data brackets, type-level computation is user-definable, and metadata about parameters is cleanly separated from types?

## Current State

Type annotations use `@` to attach type information. The current mechanisms:

```tinct
x@Number                              # shorthand — type only
x@[type: Number  default: 30]         # full form — type + metadata
[fn@Number [x@Number] ...]            # return type shorthand
[fn@[return: Number  doc: "Sum"] ...]  # full form
```

The `@` annotation today resolves bracket contents through an ad-hoc resolver in `typecheck_annot.rs` that handles a fixed set of type names and constructors. There is no way for users to define new type-level operators. Union types require `Type::Union` but there is no annotation syntax to express them. The metadata keys (`type:`, `default:`, `doc:`) are hard-coded.

## Why V2 Matters for tinct

**User-definable type operators.** `--- stage: type` sections define functions that run at type-check time. The annotation resolver evaluates bracket contents in the type-stage environment — so `@[or Int Null]`, `@[Seq Int]`, and `@[Map String: Int]` all call type-stage functions that produce type dicts.

**Composable union and intersection types.** `or` and `each` are type-stage functions, not built-in annotation forms. `x@[or Int Null]` calls the `or` type-stage function. `x@[each Comparable Showable]` calls `each`. No special syntax needed.

**Clean metadata separation.** Parameter metadata (`default:`, `doc:`, `is:`, `repr:`) lives alongside the type in a single `@[...]` bracket, under recognized keys. Unknown keys are ignored. This is already the model — V2 formalizes it and adds `constraint:` for TypeVar constraints.

**TypeVar scoping via `bind:`.** Multi-TypeVar functions use `bind: [a b c]` as the explicit TypeVar declaration site, processed before all other keys. TypeVars are local to the function annotation.

## Design

### Annotation Forms

**Parameter — single bracket, type under `type:` key when mixed with metadata:**

```tinct
x@Int                                  # simple type (shorthand — unchanged)
x@[or Int Null]                        # union type (type-only, or combinator)
x@[host: String  port: Int]            # Record type (all-keyed, no reserved keys)
x@[type: Int  default: 10]             # type + metadata
x@[type: [or Int Null]  default: 0]    # union + metadata — type: holds the type-stage expr
xs@[type: [Seq Int]  default: []]      # Seq + default
radix@[type: Int  default: 10  doc: "Numeric base"  is: positive?]
```

**Function — single bracket, return type under `return:` key:**

```tinct
fn@String                              # shorthand (unchanged)
fn@[return: String]                    # equivalent, explicit
fn@[return: String  doc: "Format a greeting"]
fn@[return: [or Int Null]]             # union return type
fn@[return: a  constraint: [a: Comparable]]
fn@[bind: [a b c]  return: c  constraint: [a: Numeric  b: Numeric  [$Addable a b c]]]
```

**Reserved keys** in annotation brackets: `type:`, `return:`, `default:`, `doc:`, `is:`, `repr:`, `constraint:`, `bind:`, `kinds:`. Records using these field names must use a type alias or `@[type: [record fieldname: T ...]]`.

### fn@[...] Metadata Dict Keys

| Key | Value | Semantics |
|-----|-------|-----------|
| `bind:` | `[a b c]` positional — TypeVar names | Declares fresh TypeVars; processed FIRST |
| `return:` | any type-stage expression | The function's return type |
| `constraint:` | `[typevar: ClassName ...]` keyed + MPTC positional | TypeVar class constraints and MPTC relations |
| `kinds:` | `[f: Operator  key: Label]` keyed | Kind constraints on declared TypeVars |
| `doc:` | string literal | Documentation string — LSP hover, not type-checked |

**Processing order** (fixed, source order irrelevant):
1. `bind:` — declares TypeVars in `ann_mapping`
2. `kinds:` — registers kind constraints on declared TypeVars
3. `constraint:` keyed entries — class constraints on TypeVars
4. `constraint:` MPTC positional entries — relate TypeVars
5. `return:`/`type:` — reference declared TypeVars
6. `doc:`, `default:`, `is:`, `repr:`, arbitrary keys — runtime metadata

### TypeVar Scoping via `bind:`

`bind:` is the sole TypeVar declaration site. Lowercase names in annotation position are always TypeVars, never value references. No outer-scope shadowing possible.

```tinct
# Single TypeVar
min: [fn@[bind: [a]  return: a  constraint: [a: Comparable]]  [xs@[Seq a]] ...]

# Two TypeVars
compare: [fn@[bind: [a b]  return: Bool  constraint: [a: Comparable  b: Showable]]
          [x@a  y@a  logger@b] ...]

# MPTC — all TypeVars declared in bind:; [$Addable a b c] relates them
add-typed: [fn@[bind: [a b c]  return: c
                constraint: [a: Numeric  b: Numeric  [$Addable a b c]]]
  [x@a  y@b]  [+ x y]]
```

Rules:
1. `bind:` registers names as fresh TypeVars in `ann_mapping`; processed first.
2. `return:`, `type:`, and parameter `@` annotations are reference-only — they look up names already in `ann_mapping`. A name not in `bind:` is a type error.
3. MPTC positional entries `[$Add a b c]` are purely relational — all names must be in `ann_mapping`.

### Union and Intersection Types

**Union — `or` type-stage combinator:**

```tinct
x@[or Int Null]                  # Int | Null
x@[or String Int Bool]           # String | Int | Bool
fn@[return: [or Int Null]]       # function returning Int | Null
x@[type: [or Int Null]  default: 0]  # union + metadata
```

`or` is a type-stage function in the prelude: `or: [fn [...types] [kind: "union"  members: types]]`. Annotation resolver evaluates `[or Int Null]` in the type-stage Env and receives `[kind: "union" members: [...]]` → `Type::Union([Int, Null])`.

The OLD positional form `x@[Int Null]` is **retired** — it has fatal ambiguity with parameterized type constructors (`[Seq Int]` = Seq of Int, not Seq | Int). Use `x@[or Int Null]` instead.

**Intersection — `each` type-stage combinator:**

```tinct
x@[each Comparable Showable]         # Comparable ∩ Showable
constraint: [a: [each Comparable Showable]]   # TypeVar a must satisfy both
```

`each` produces `[kind: "inter"  members: [...]]` → `Type::Intersection([...])`. In `constraint:` position, intersection types become multiple `Constraint::Class` entries. In type annotation position, they produce `Type::Intersection`.

Multi-class constraint alternative — `[each ...]` preferred:
```tinct
# RETIRED (reads as implied call):
constraint: [a: [Comparable Showable]]

# CORRECT:
constraint: [a: [each Comparable Showable]]
```

### Kind Constraints via `kinds:`

```tinct
# In class declaration — structural bracket
Functor: [class [f]  [kinds: [f: Operator]]
  fmap: [fn@[return: [f b]] [[Fn@b [a]]  [f a]]]]

# In function annotation
fmap-generic: [fn@[bind: [a b f]
                   kinds: [f: Operator]
                   constraint: [f: Functor]
                   return: [f b]]
  [fn@b [a]  xs@[f a]] ...]
```

`kinds:` is processed after `bind:` and before `constraint:`. Kind names: `Operator` (type constructor `* → *`), `Label` (dict field key), future: `Monad`, `Functor`.

### Type-Stage Evaluation

Annotation brackets `@[...]` are evaluated in the type-stage Env (built from `--- stage: type` sections). The annotation resolver:

1. Receives the bracket expression as an `Expr`
2. Calls `eval(expr, type_stage_env)` → a type dict (Value::Dict)
3. Converts the type dict to `Type::*` via `dict_to_type()`

Name resolution order in the type-stage Env: type-stage bindings → type alias table → primitive named types (`Int`, `String`, `Bool`, `Null`, `Any`, `Unknown`).

**Recursive type-stage functions are not supported.** The lazy evaluator defers self-calls as thunks; the annotation resolver forces thunks during traversal, causing infinite unrolling until the 256-layer depth limit fires. Recursive types use `μ`-types (equirecursive, `isorecursive-types` whatif).

### `is:` in Match Patterns — Soft Guard

`x@[is: pred]` in a match arm is a **soft guard** (distinct from parameter `is:` which is a hard error):

```tinct
[match value
  n@[is: positive?]: [str "positive: " n]  # positive? falsy → skip arm
  n@Int:             [str "non-positive: " n]]
```

- `pred(x)` truthy → arm matches, `x` bound in body
- `pred(x)` falsy → arm skipped, next arm tried (soft)
- `pred(x)` throws → hard runtime error propagates (not a skip)

Parameter `is:` (in fn parameter list) is always a hard runtime error on failure — never falls back to `default:`.

### `constraint:` Evaluation

`constraint:` values are evaluated in the type-stage Env:

```tinct
constraint: [a: Comparable]               # Comparable → [kind: "named" name: "Comparable"]
constraint: [a: [each Comparable Showable]]  # each → [kind: "inter" members: [...]]
constraint: [a: Comparable  b: Showable]  # two TypeVars
```

Routing:
- `[kind: "named" name: N]` → `Constraint::Class(N, α)`
- `[kind: "inter" members: [...]]` → one `Constraint::Class` per member
- `[kind: "union" members: [...]]` → `Constraint::Any([...])` (rare)
- Any other kind → type error: "invalid constraint expression"

### `[type ...]` Bodies are Type-Stage

`[type ...]` body expressions use the full annotation resolver with type-stage Env access:

```tinct
NullableInt: [type [or Int Null]]         # calls or type-stage fn
Scores:      [type [Map String: Int]]      # calls Map type-stage constructor
Either:      [type [a b] [or a b]]        # parameterized — a, b as TypeVars in body
```

**Exception:** Nominal variants `[type [Ok a] [Err String]]` are registered structurally by the type-checker, not evaluated as type-stage expressions.

## Type Dict Schema

Type-stage functions return type dicts. Schema for all valid `kind:` values:

| `kind:` value | Fields | tinct `Type::*` |
|---------------|--------|-----------------|
| `"named"` | `name: String` | `Type::Int`, `Type::Str`, `Type::Bool`, `Type::Unknown`, named aliases |
| `"union"` | `members: [...]` | `Type::Union(Vec<Type>)` |
| `"inter"` | `members: [...]` | `Type::Intersection(Vec<Type>)` |
| `"seq"` | `element: <type-dict>` | `Type::Seq(Box<Type>)` |
| `"map"` | `key: <type-dict>  value: <type-dict>` | `Type::Map(Type, Type)` |
| `"fn"` | `return: <type-dict>  params: <seq-dict>` | `Type::Function { ret, params }` |
| `"record"` | `fields: {name: <type-dict> ...}` | `Type::Record(Row)` |
| `"recursive"` | `var: String  body: <type-dict>` | `Type::Recursive { var, body }` |
| `"recvar"` | `name: String` | `Type::RecVar(String)` |

**Bare uppercase rule:** Unconstrained type positions use `Unknown` (gradual `?`), not `Any`. Exception: `Fn` params use `Any` to express variadic-Any calling convention.

## Resolved Questions

**Q1: `or:` vs `or` in annotation brackets.**
`or:` (colon-suffixed) = always a dict key = always a Record field. `or` (bare in head position) = always a type function call. `@[or: Int  port: Int]` → Record with fields "or" and "port". `@[or Int Null]` → union type. Same bracket disambiguation rule as `[fn [x] x]` vs `[fn: x]`. No special annotation disambiguation.

**Q2: Record collision with reserved keys.**
A record type with field names that match reserved keys (`type:`, `return:`, etc.) must use a type alias or `@[type: [record fieldname: T ...]]`. The double-@ form `@Record@[...]` is removed — it was the proposed workaround, but it produced ambiguous parsing. Type aliases are the intended solution.

**Q3: Macros vs type-stage are orthogonal.**
Macros operate at parse/expansion time (code → code). Type-stage operates at type-check time (type → type). They compose: macros can produce annotations containing type-stage expressions. Type-stage does not replace macros.

**Q4: `default:` and `is:` are not a clamp.**
`default:` substitutes when an argument is absent (not provided). `is:` validates when an argument IS present; failure → runtime error, never falls back to `default:`. A caller providing a value that fails `is:` gets a runtime error.

**Q5: Multi-class constraint syntax.**
`[a: [Comparable Showable]]` is RETIRED — reads as implied call `Comparable(Showable)`. Use `[a: [each Comparable Showable]]` instead.

**Q6: Type-stage Env isolation.**
Separate Env built from `--- stage: type` sections (prelude first, then program). Main evaluator reused unchanged. No mode flag. `type_stage_env` discarded when type-checking completes; not present at runtime.

## What Would Change

### `src/ast.rs` — `Annotation` variants
Add `Annotated(String, Box<Annotation>)` for chained annotations. Already implemented.

### `src/typecheck_annot.rs` — Annotation resolver
**fn@[...] path:** Inspect PropertyDict for recognized metadata keys (`return:`, `constraint:`, `bind:`, `kinds:`, `doc:`). If any present → `resolve_fn_metadata()`. If all positional → type expression resolved via type-stage Env.

**resolve_fn_metadata processing order:**
1. `bind:` — declare TypeVars in `ann_mapping`
2. `kinds:` — register kind constraints
3. `constraint:` keyed entries — `Constraint::Class` per entry
4. `constraint:` MPTC positional `[$Class a b c]` — lookup class in ClassEnv
5. `return:`/`type:` — resolve as type-stage expression, look up TypeVars in `ann_mapping`
6. `doc:` — store string in TypeScheme.doc

**Parameter annotations:** When annotation has positional entries and no `type:` key → resolve as type-stage expression (e.g., `@[or Int Null]`). When `type:` key present → evaluate `type:` value as type-stage expression.

### `src/typecheck.rs` and `stdlib/prelude.llt` — Type prelude
Add `--- stage: type` section to prelude with `or`, `each`, `Seq`, `Map`, `record`, `Fn`, `kind`, `fn` type-stage combinators and ground type dicts for `Int`, `String`, `Bool`, `Null`, `Any`, `Unknown`.

### `src/type_dict.rs` (new file)
`pub fn type_to_dict(ty: &Type) -> Value` and `pub fn dict_to_type(val: &Value, span: Span) -> Result<Type, TypeError>`. Cover all `Type` variants. `dict_to_type` errors on unknown `kind:` or missing fields.

### `src/typecheck_annot.rs` — `kinds:` routing
Add `kinds:` as recognized metadata bracket key alongside `constraint:`. Route to `kind_env` after `bind:` processing. Retire `f@Operator` annotation form; `kinds:` is canonical.

### `src/ast.rs` / `src/parser.rs` — `Expr::ClassDecl` fields
Add `determines: Vec<Spanned<Expr>>` and `resolver: Option<Spanned<Expr>>` to `Expr::ClassDecl` for routing out of the `methods` list. `StackFrame::ClassDecl` gains matching fields.

### Migration path
1. Retire `@[T1 T2]` positional union → `@[or T1 T2]` (mechanical, 8 corpus files)
2. Retire `[a: [Comparable Showable]]` → `[a: [each Comparable Showable]]` (mechanical)
3. Add `bind:` to multi-TypeVar fn annotations in prelude and stdlib
4. Update `doc/05-type-annotations.md` constraint syntax and examples

## Prerequisites

- `type-stage-infra` sprint — `--- stage: type` section header, type dict schema, `type_to_dict`/`dict_to_type`, type prelude bootstrap

## References

- Jones, M.P. (1995). *Qualified Types: Theory and Practice.* Cambridge University Press. — [principal types with constraint schemes; processing order for constraint resolution]
- Jones, M.P. (2000). "Type Classes with Functional Dependencies." *ESOP 2000*, LNCS 1782, pp. 230–244. — [fundep improvement; TypeVar binding in multi-param constraints]
- Sulzmann, M., Duck, G.J., Peyton Jones, S. & Stuckey, P.J. (2007). "Understanding Functional Dependencies via Constraint Handling Rules." *JFP*, 17(1), 83–137. — [CHR framework for constraint resolution; propagation and simplification rules]
- Siek, J.G. & Taha, W. (2006). "Gradual Typing for Functional Languages." *Scheme and Functional Programming Workshop.* — [Unknown as gradual type; is_consistent; TypeVar treatment in gradual system]
- Gaster, B.R. & Jones, M.P. (1996). "A Polymorphic Type System for Extensible Records and Variants." Technical Report NOTTCS-TR-96-3. — [named parameters in calling convention; names not part of principal type]
