# What If: Runtime Type System — Continuous Narrowing from Static to Dynamic

**State:** Accepted — 2026-07-28

Static type checking and runtime type checking are two sides of the same coin. The same type representation — a tinct `Value` — flows through every phase: inference, lowering, evaluation, and pattern matching. The `Type::*` Rust enum is eliminated.

## The Central Principle

Every `Value` in tinct carries a `type_val: Arc<Value>` field. This field IS the type. There is no separate `Type` Rust enum, no conversion layer, no dual representation. The inference engine operates on `Arc<Value>` throughout. TypeAssert nodes, MethodDispatcher arms, and runtime type checks all read from `value.type_val`.

Static type checking writes to `InferenceContext`. Runtime reads it (frozen). This is not two separate systems glued together — it is one system with a read-write phase and a read-only phase.

## The Rust/Tinct Boundary

Every point where Rust and tinct types intersect is explicit and tinct-owned:

| Crossing Point | Direction | Mechanism |
|---|---|---|
| Rust `Value::Int` → tinct `NativeInt` | Rust → tinct | Resolved via `repr: "Value::Int"` in `builtin_core.llt`; Rust reads at load time |
| tinct `[@NativeInt x]` → validates `x.type_val == NativeInt` | tinct → Rust | `CoreExpr::TypeAssert { check: Resolved(arc_native_int) }` in eval |
| Dispatch `[< a b]` with polymorphic `<` | tinct → tinct | MethodDispatcher tinct function matching on `a.type_val` |
| `[type repr: "Value::Int"]` declares coupling | tinct → Rust | `repr_registry` validated at declaration time; invalid strings error |
| `builtin-type-of x` returns `x.type_val` | Rust → tinct | Builtin reads `value.type_val`, returns it directly |
| Rust builtins produce typed values | Rust → tinct | Return-type annotation in `builtin_core.llt` resolved at load time; Rust function has zero hardcoded type names |

## Type — The Metatype

TypeValue is a tinct sum type declared in `stdlib/builtin_core.llt`. It is the type of types. Every tinct value's `type_val` field points to a TypeValue instance.

```tinct
# In stdlib/builtin_core.llt
# Dict used for all list-like fields — finite, eager, concrete.
# [] for absent/null — Null is not defined yet at this point.
TypeValue: [type
    Repr:         [repr: String  is: [or [] Fn]]     # Rust-backed; is: = validity predicate
    Phantom:      []                                  # no-payload phantom type
    Var:          [name: String]                      # inference variable; level in InferenceContext
    Fn:           [params: Dict  return: TypeValue]   # function TypeValue; self-referential
    App:          [op: TypeValue  arg: TypeValue]     # type application T[A]
    Op:           [name: String]                      # type operator (type constructor name only)
    Record:       [fields: Dict  tail: [or [] TypeValue]]  # structural record; [] = closed
    Union:        [members: Dict]                     # BAS union
    Inter:        [members: Dict]                     # BAS intersection
    Neg:          [of: TypeValue]                     # BAS negation
    Scheme:       [vars: Dict  constraints: Dict  body: TypeValue]  # polymorphic scheme
    IntLit:       [value: Integer]                    # integer literal type
    FloatLit:     [value: Float64]                    # float literal type
    StrLit:       [value: String]                     # string literal type
    Recursive:    [body: TypeValue]                   # μ-type; de Bruijn, no var name
    RecursiveRef: [depth: Integer]                    # 0 = innermost binder, 1 = one out
    Unknown:      []                                  # gradual ? type (AGT consistency)
    Never:        []                                  # bottom; divergence and inference failure
    Top:          []]                                 # all values; every type <: Top
```

`TypeValue` references itself in field types. This is a valid self-referential nominal type in tinct.

`Seq` is not available at bootstrap — all list-like fields use `Dict`. `Null` is not defined at bootstrap — `[]` is used for absent optional fields.

### TypeValue Identity

Two `[type repr: "Value::Float"]` declarations in different scopes produce distinct TypeValue objects. `Temperature` and `Float64`, both backed by `Value::Float`, are fully distinguishable — `[match x Float64: ... Temperature: ...]` compares `x.type_val` by object identity. The `repr:` string names Rust storage; it does not auto-tag values.

### RowTail

Rows and types are distinct syntactic categories (Rémy 1989). RowTail is a separate sum type from TypeValue:

```tinct
RowTail: [type
    Closed:  []                                          # no additional fields
    Var:     [name: String]                              # row variable; level in InferenceContext
    Uniform: [key: [or [] TypeValue]  value-type: TypeValue]]  # key: [] = any key; key: T = typed-key
```

`TypeValue.Record` uses `[or [] TypeValue]` for its `tail` field, where the TypeValue must be a RowTail. The `RowTail.Uniform` `key:` field encodes the Map[K V] constraint: `key: []` means any key (was `key: None`); `key: K` means typed-key constraint (was `key: Some(K)`).

### Supporting TypeValue Declarations

```tinct
# Kinds are TypeValues
Row: [type]   # phantom TypeValue; marks row-kinded variables

# In Scheme vars
VarDecl: [type [name: String  kind: TypeValue]]

# In Scheme constraints
ConstraintDecl: [type [class: TypeValue  args: Dict]]
```

Kinds are TypeValues: `Type = TypeValue.Op "Type"` (the kind `*`), `TypeValue.Fn` with `params: [Type]` and `return: Type` is the kind `* → *`, `Row` is the row kind.

## Runtime Type Identity

`builtin-type-of` returns a value's `type_val` directly:

| Value | `builtin-type-of` returns |
|---|---|
| `42` | `NativeInt` (TypeValue.Repr with `repr: "Value::Int"`) |
| `3.14` | `Float64` (TypeValue.Repr with `repr: "Value::Float"`) |
| `"hello"` | `String` (TypeValue.Repr with `repr: "Value::String"`) |
| `[a: 1]` | `Dict` (TypeValue.Repr with `repr: "Value::Dict"`) |
| `Color.True` | `Color.True` variant TypeValue (booleans are nominal ADTs, not Repr) |
| `Color.Red` | The TypeValue of the declared type (`Color`) |
| `[fn [let x] x]` | TypeValue.Fn describing the function's signature |
| `[@NativeInt 42]` | `NativeInt` after TypeAssert narrows |

`builtin-type-of` is a pure lookup — it reads `value.type_val` and returns it. No string conversion, no type name lookup.

## Deferred Dispatch — MethodDispatcher

Polymorphic typeclass methods that cannot be resolved statically (TypeVar not yet ground) are handled by a MethodDispatcher: a tinct function emitted by the lowerer at every typeclass method site.

```tinct
# Emitted by the lowerer for the < method with known instances
<: [fn [let x y]
    [match x
      NativeInt: [ɪɴꜱᴛᴀɴᴄᴇ⧼Sortable∷<⟨NativeInt⟩⧽ x y]
      Float64:   [ɪɴꜱᴛᴀɴᴄᴇ⧼Sortable∷<⟨Float64⟩⧽  x y]
      String:    [ɪɴꜱᴛᴀɴᴄᴇ⧼Sortable∷<⟨String⟩⧽   x y]
      ...:       [raise "no Sortable instance for <"]]]

# After [instance Sortable [let k@MyType]: ...], user scope wraps with delegation
<: [fn [let x y]
    [match x
      MyType: [ɪɴꜱᴛᴀɴᴄᴇ⧼Sortable∷<⟨MyType⟩⧽ x y]
      ...:    [.< x y]]]    # .< delegates to parent scope
```

Key properties:
- Match directly on `x` — NOT `[match [builtin-type-of x] ...]` (redundant intermediate call)
- Arms use concrete types (`NativeInt`, `Float64`) — NOT abstract typeclasses (`Integer`, `Float`)
- Scope delegation via `.<` (leading-dot parent-scope access) — correct tinct idiom
- No `Value::MethodDispatcher` Rust variant — plain tinct function, no special treatment
- No `dispatch_prefix` string manipulation

`call_dispatch` and `DispatchObligation` are eliminated. `call_dispatch` was a divergent execution path attempting to resolve dispatch at type-check time via a parallel Rust mechanism. The MethodDispatcher replaces it with one correct execution path for all cases.

## Type Patterns in `[match]`

TypeValues appear in match arm patterns to dispatch on the runtime type of a value. The match arm compares `scrutinee.type_val` against the pattern TypeValue by reference/structural identity:

```tinct
[match x
  NativeInt: [str "integer: " [str x]]
  Float64:   [str "float: "   [str x]]
  String:    [str "string: "  x]
  ...:       "other"]
```

This is not a special mechanism — it is ordinary match using `type_val`. Abstract typeclass patterns check the instance registry.

## Shared Type-Narrowing Engine — InferenceContext

```tinct
InferenceContext: [type [
    levels:    Dict    # TypeVar name → creation-time level (Integer)
    subst:     Dict    # TypeVar name → TypeValue binding (monotonic — bound once only)
    instances: Dict    # class × TypeValue → instance method dict
]]
```

`InferenceContext` is the shared mutable state across incremental static passes. TypeVar binding is monotonic: each variable is bound at most once in `subst`. A second binding attempt raises an error. Cascade prevention: failed TypeVars are tracked so errors don't multiply.

**Read-only at runtime.** Static type-checking passes write to `InferenceContext`. Evaluation reads it (frozen). This is mandatory for thunk referential transparency — a thunk must produce the same value on every force, which requires that the context it reads does not change after it is first forced. Unbound TypeVars encountered at runtime evaluate to `TypeValue.Unknown`.

`TypeValue.Var` carries identity only (`name: String`). The `levels` field in `InferenceContext` is the mutable side — TypeVar level lowering mutates the context, not the TypeValue. Since `Arc<Value>` is immutable, level must not live in TypeValue.

---

## Dead Code Analysis — Integrated Liveness

Lost-binding warnings ("variable 'x' is never referenced") require knowing which bindings transitively contribute to the program's output. This is a standard liveness analysis over a use-def graph.

**Why a separate dep_graph is wrong.** The prior implementation maintained a parallel `dep_graph: Vec<HashSet<String>>` in the `AfterSequentialFinal` CEK continuation — a per-dict, cross-dict-only approximation of binding dependencies. Both coarsenesses produced wrong results:

- **False positives** (warns when it shouldn't): intra-dict edges are absent. `run-test` (Dict 2) referencing `run-test-file` (Dict 2) — both in the same intermediate dict — is not captured. When `run-test` is live, `run-test-file` is not made live, producing a spurious warning.
- **False negatives** (misses warnings): per-dict granularity means that if dead binding `a` and live binding `b` share the same intermediate dict, all names from earlier dicts that either referenced become live — including those referenced only by the dead `a`.

The dep_graph is also a shadow of information the type checker already computes: every VarRef resolution marks `slot.referenced`. The use-def graph is implicit in that mark sequence.

**The correct integration: record edges at VarRef time.**

The type-check pass maintains two fields in `InferState` (the local per-pass state, not the persistent `InferenceContext`):

```rust
current_binding: Option<String>,               // which dict entry is currently being inferred
use_def:         HashMap<String, HashSet<String>>,  // use_def[name] = names name's expression referenced
```

In `run_typecheck_dict`, each entry's inference is bracketed:

```rust
state.current_binding = Some(entry_name.clone());
// ... type-check entry value ...
state.current_binding = None;
```

At the VarRef resolution site — alongside the existing `referenced` mark — the edge is recorded for free:

```rust
if let Some(ref binder) = state.current_binding {
    if binder.as_str() != name {
        state.use_def.entry(binder.clone()).or_default().insert(name.to_string());
    }
}
```

`AfterSequentialFinal` then performs a BFS on `use_def`, replacing the backward fixpoint entirely:

```
live  ← names the final expression directly referenced
queue ← live
while queue not empty:
    name ← dequeue
    for dep in use_def[name]:
        if dep ∉ live: live ← live ∪ {dep}; enqueue dep
warn: bindings with definition_span not in live
```

**Precision.** Per-binding edges capture intra-dict references correctly — they are recorded at VarRef time regardless of which dict both names share. BFS propagates liveness transitively and per-binding: if `run-test` is live, `run-test-file` becomes live through `use_def["run-test"]`. Dead binding `a` referencing `x` does not drag `x` into the live set unless `a` itself is reachable from the final expression.

**Scoping.** `current_binding` and `use_def` are scoped to the current Sequential. They are reset at the start of each Sequential and the accumulated `use_def` is consumed by `AfterSequentialFinal`. They do not persist across incremental passes and are not part of `InferenceContext`.

**Fit with the runtime-types design.** `use_def` tracks names, not types. The edge-recording at VarRef sites is orthogonal to whether types are `Type` enums or `Arc<Value>`. VarRef resolution happens in both worlds; the `current_binding` context rides alongside without interfering. No new type-theoretic machinery is required.

---

## Type Assertions and Conversions

`[@T x]` is a type CHECK, not a conversion. Two cases:

1. **Pending literals** — `x` is a numeric/string literal not yet materialized. TypeAssert sets an annotation slot in the CEK machine before forcing `x`'s inner expression. Literal materialization reads the slot and uses `T` as `type_val`. Slot is cleared after forcing.

2. **Already-typed values** — TypeAssert validates that `x.type_val` is consistent with `T`. Fails if mismatch. Never retypes.

`Castable` / `[cast x]` handles conversions. The target type is inferred from context — `[cast x]` not `[cast T x]`. TypeAssert is for checking; cast is for converting.

## Type Predicates

Type predicates carry `narrows:` annotations that inform the type checker:

```tinct
int?:   [fn@[or 0 1] [let x@Any narrows: NativeInt]  [builtin-int?   x]]
float?: [fn@[or 0 1] [let x@Any narrows: Float64]     [builtin-float? x]]
str?:   [fn@[or 0 1] [let x@Any narrows: String]      [builtin-str?   x]]
bool?:  [fn@[or 0 1] [let x@Any narrows: Boolean]     [builtin-bool?  x]]
dict?:  [fn@[or 0 1] [let x@Any narrows: Dict]        [builtin-dict?  x]]
bytes?: [fn@[or 0 1] [let x@Any narrows: Bytes]       [builtin-bytes? x]]
```

`narrows: T` tells the type checker that in the true branch of an `if` using this predicate, the scrutinee has type `T`. The type checker uses this for path-sensitive narrowing.

## Abstract and Concrete Types

### The Distinction

**Abstract types** are typeclasses — they express what a value CAN DO:
- `Numeric` — any numeric type
- `Integer <: Numeric` — any integer representation  
- `Float <: Numeric` — any floating-point representation

**Concrete types** use `repr:` — they express HOW a value is stored:
```tinct
NativeInt:          [type repr: "Value::Int"]      # signed 64-bit machine integer
NativeUInt:         [type repr: "Value::U64"]      # unsigned 64-bit machine integer
Float64:            [type repr: "Value::Float"]    # IEEE 754 double
BigInt:             [type repr: "Value::BigInt"]   # arbitrary precision integer
```

### Numeric Hierarchy

`Numeric`, `Integer`, and `Float` are marker typeclasses with no own methods — only superclasses. They follow the `Comparable` pattern (already in the type checker):

```tinct
Numeric: [class [let a]]
Integer: [class [let a] [superclasses: [[Numeric a] [Addable a a a] [Equatable a] [Sortable a]]]]
Float:   [class [let a] [superclasses: [[Numeric a] [Addable a a a] [Sortable a]]]]
```

Empty-method instances declare class membership:
```tinct
[instance Integer [let k@NativeInt]:  []]
[instance Integer [let k@NativeUInt]: []]
[instance Integer [let k@BigInt]:     []]
[instance Float   [let k@Float64]:    []]
```

**Use `@Integer` for all normal code.** Concrete types (`NativeInt`, `NativeUInt`) are only for binary protocols, FFI, and wire formats where the exact storage size and layout matter.

### Arithmetic Behavior

`repr:` declarations are building blocks. Whether `NativeUInt + NativeUInt` wraps, saturates, checks, or promotes is determined by which `Addable` instance is in scope — a library concern, not specified here.

## Phantom Types

A `[type]` declaration with no body produces a `TypeValue.Phantom`. Values of a phantom type can only be created via type assertion `[@T x]` — there is no constructor, no `repr:`, no structural access:

```tinct
UserId: [type]     # phantom type; TypeValue.Phantom

# Values created only by assertion
user-id: [@UserId some-integer]    # wraps some-integer with UserId type_val
```

TypeValue is never stripped silently. `[@UserId user-id].type_val == UserId`. This enables compile-time distinction between values that have the same Rust storage but semantically different roles.

## Declaring Rust-Backed Types

The `repr:` key in a `[type ...]` body names the Rust `Value::*` enum variant that backs this type. The string must exactly match a known `Value::*` variant — unrecognized strings error at declaration time.

All `repr:` declarations live in `stdlib/builtin_core.llt`. This is the only place Rust storage names appear in tinct code.

| tinct declaration | Rust storage | Notes |
|---|---|---|
| `NativeInt: [type repr: "Value::Int"]` | `Value::Int { n: i64, .. }` | Signed 64-bit |
| `NativeUInt: [type repr: "Value::U64"]` | `Value::U64 { n: u64, .. }` | Unsigned 64-bit |
| `Float64: [type repr: "Value::Float"]` | `Value::Float { n: f64, .. }` | IEEE 754 double |
| `String: [type repr: "Value::String"]` | `Value::String { .. }` | Interned |
| `Bytes: [type repr: "Value::Bytes"]` | `Value::Bytes { .. }` | |
| `BigInt: [type repr: "Value::BigInt"]` | `Value::BigInt { .. }` | Arbitrary precision |
| `Decimal: [type repr: "Value::Decimal"]` | `Value::Decimal { .. }` | |
| `Dict: [type repr: "Value::Dict"]` | `Value::Dict { .. }` | The sole collection primitive |
| `DirCap: [type repr: "Value::DirCap"]` | `Value::DirCap { .. }` | |
| `NetCap: [type repr: "Value::NetCap"]` | `Value::NetCap { .. }` | |
| `ClockCap: [type repr: "Value::ClockCap"]` | `Value::ClockCap { .. }` | |
| `File: [type repr: "Value::File"]` | `Value::File { .. }` | |
| `Program: [type repr: "Value::Program"]` | `Value::Program { .. }` | AST value |
| `Document: [type repr: "Value::Document"]` | `Value::Document { .. }` | AST value |
| `Expression: [type repr: "Value::Expression"]` | `Value::Expression { .. }` | AST value |
| `Channel: [type repr: "Value::Channel"]` | `Value::Channel { .. }` | |
| `Task: [type repr: "Value::Task"]` | `Value::Task { .. }` | |
| `Boolean` | — not a repr: type — | Boolean is a nominal ADT in prelude: `Boolean: [type True False]`. Booleans are `Value::Variant`, not a primitive storage kind. |

The `is:` field declares a value invariant checked at TypeAssert time:
```tinct
Port: [type repr: "Value::Int"  is: [between 1 65535]]

[port@Port: 80]       # type-annotated field
[port: [@Port 80]]    # runtime assertion — calls is: predicate
```

### Localization

The tinct type name is entirely up to the declaring program. Rust knows only `"Value::Int"`:
```tinct
整数: [type repr: "Value::Int"]

# builtin-type-of 42 returns 整数 (not NativeInt) in this scope
```

A localized prelude wraps builtins with type assertions that remap TypeValues at the boundary. No Rust changes required.

## Function TypeValues

Functions carry structural TypeValues:

```tinct
[fn@NativeInt [let x@NativeInt y@NativeInt] [builtin-int-add x y]]
```

This function's `type_val` is:
```tinct
[TypeValue.Fn
    params: {0: [VarDecl name: "x"  kind: NativeInt]
             1: [VarDecl name: "y"  kind: NativeInt]}
    return: NativeInt]
```

`Callable` is the abstract typeclass for all callable values. `Fn` extends `Callable` with parameter/return structure. Builtin functions have `type_val` set from their `builtin_core.llt` annotation at load time.

The Rust builtin function has zero hardcoded knowledge of tinct type names. The same Rust arithmetic function under a different `[fn@MyFloat64 ...]` declaration produces `MyFloat64` values.

## Types as First-Class Values

Every `[type ...]` declaration produces a dict value that can be passed to functions, stored in data structures, and inspected:

```tinct
# TypeValue is a value like any other
types: [NativeInt Float64 String]
names: [map [fn [let t] [builtin-type-name t]] types]
```

`builtin-type-of x` returns the TypeValue — not a string. Type checks use TypeValue directly:

```tinct
# WRONG — uses string comparison (eliminated)
[if [= [builtin-type-of x] "Integer"] ...]

# CORRECT — uses TypeValue identity
[if [= [builtin-type-of x] NativeInt] ...]
```

## The Handoff Protocol Summary

At every type boundary in the pipeline, `CoreExpr::TypeAssert` is emitted:

| Boundary | TypeAssert source | TypeAssertCheck variant |
|---|---|---|
| `[@T x]` in source | Annotation in source text | `Source { annotation }` |
| Function parameter with type annotation | Lowerer reads type checker output | `Resolved(arc_typevalue)` |
| Function return type | Lowerer reads type checker output | `Resolved(arc_typevalue)` |
| TypeClass dispatch obligation | Lowerer replaces DispatchObligation | `Resolved(constraint_typevalue)` |
| Type guard in `if`/`match` | Lowerer reads `SurfaceNode.type_guard` | `Resolved(arc_typevalue)` |

TypeAssert is ALWAYS emitted at every type boundary. No elision, no fast paths.

```rust
CoreExpr::TypeAssert {
    expr: Arc<Spanned<CoreExpr>>,
    check: TypeAssertCheck,
}

enum TypeAssertCheck {
    Source { annotation: Spanned<Annotation> },
    Resolved(Arc<Value>),  // TypeValue known at lowering time
}
```

## Boundaries of Responsibility

| Layer | Owns | Does not own |
|---|---|---|
| `stdlib/builtin_core.llt` | TypeValue declaration; repr: table; RowTail; VarDecl; ConstraintDecl | Type inference algorithms |
| `stdlib/prelude.llt` | Numeric hierarchy classes/instances; type predicates with narrows:; MethodDispatcher emission sites; Castable/cast | Rust storage names |
| Lowerer (`src/lower.rs`) | Emitting TypeAssert nodes; MethodDispatcher construction; Resolved(arc) filling | Type inference |
| Type checker | Writing to InferenceContext; emitting SurfaceNode.type_guard | Runtime evaluation |
| Evaluator | Reading InferenceContext (frozen); enforcing TypeAssert; reading type_val | Updating InferenceContext |
| Rust builtins | Producing values with correct type_val (from builtin_core.llt annotation) | Knowing tinct type names |

## Progressive Narrowing Protocol

Every TypeAssert narrows the type, never widens it. The TypeValue annotation can carry:

- **Concrete (ground type)** → O(1) reference identity check
- **Abstract typeclass** → `resolve_instance(class, value.type_val, instances)` via shared narrowing engine
- **BAS Union** → validate `value.type_val ∈ {A, B}`, then collapse to concrete `value.type_val`
- **BAS Intersection** → validate both constraints
- **BAS Negation** → validate exclusion
- **TypeValue.Var** → attempt runtime unification (read-only: Unknown if unbound)
- **TypeValue.Unknown** → no validation

Union refinement: `[or NativeInt Float64]` annotation at runtime collapses to whichever concrete type the value actually has. Runtime only narrows, never widens.

## What Would Change

### `src/value.rs`

**Current:** `Value::*` variants carry no type information. A separate `Type` Rust enum represents types.

**Proposed:** Every `Value::*` variant gains `type_val: Arc<Value>`. The `Type` Rust enum is eliminated entirely. All inference, checking, and dispatch operate on `Arc<Value>`:

```rust
// Before (illustrative)
enum Value { Int { n: i64 }, String { .. }, Dict { .. }, Variant { tag, payload }, ... }
enum Type { Int, Float, Any, Union(Vec<Type>), TyCon(String), ... }

// After — every variant (except Builder) gains type_val: Arc<Value>
struct TypeVal(Arc<Value>);  // newtype for clarity

enum Value {
    Int    { n: i64,        type_val: TypeVal },
    U64    { n: u64,        type_val: TypeVal },
    Float  { n: f64,        type_val: TypeVal },
    String { source, start, end, type_val: TypeVal },
    Bytes  { data: Arc<[u8]>, type_val: TypeVal },
    BigInt { n: Arc<BigInt>, type_val: TypeVal },
    Dict   { map: IndexMap<HashableValue, ThunkId>, type_val: TypeVal },
    Variant { tag: String, payload: Option<ThunkId>, type_val: TypeVal },
    Function { ... type_val: TypeVal },
    Builtin  { ... type_val: TypeVal },
    DirCap   { dir: Arc<cap_std::fs::Dir>, type_val: TypeVal },
    NetCap   { entries: Arc<Vec<NetCapEntry>>, type_val: TypeVal },
    File     { file: Arc<RefCell<cap_std::fs::File>>, type_val: TypeVal },
    Builder(Arc<Builder>),   // bootstrap sentinel only; no type_val field
    Channel  { inner: Arc<Channel<ThunkId>>, type_val: TypeVal },
    Task     { handle: Arc<JoinHandle<...>>, type_val: TypeVal },
    Program  { ast: Arc<SurfaceFile>, type_val: TypeVal },
    Document { ast: Arc<SurfaceDoc>, type_val: TypeVal },
    Expression { node: Arc<SurfaceNode>, type_val: TypeVal },
    // ... etc
}
```

The `Type` enum (`src/type_def.rs`, `src/type_env.rs`, `src/type_infer.rs`, `src/type_unify.rs`, `src/type_normalize.rs`) is eliminated. All functions that currently take `Type` arguments take `Arc<Value>` instead.

**Impact:** Very large — every use of `Type::*` in the codebase is affected.

### `src/ast.rs`

**Current:** `CoreExpr::TypeAssert { resolved_type: Type }` carries a `Type`.

**Proposed:**
```rust
CoreExpr::TypeAssert {
    expr: Arc<Spanned<CoreExpr>>,
    check: TypeAssertCheck,
}

enum TypeAssertCheck {
    Source { annotation: Spanned<Annotation> },
    Resolved(Arc<Value>),
}
```

**Also:** `call_dispatch`, `DispatchObligation`, `TypeAnnotationTable` — all eliminated.

### `src/type_infer.rs` (new: `src/inference_context.rs`)

**Current:** `InferState` with `type_vars: HashMap<String, TypeVarEntry>`, `Substitution` with `type_map: RefCell<HashMap<String, Type>>`.

**Proposed:** `InferenceContext` (persistent, shared across incremental passes):
```rust
struct InferenceContext {
    levels:    HashMap<String, u32>,       // TypeVar name → creation level
    subst:     HashMap<String, Arc<Value>>, // TypeVar name → TypeValue binding (monotonic)
    instances: InstanceRegistry,           // class × TypeValue → instance dict
}
```

`InferState` (local per-pass wrapper around `InferenceContext`) gains two fields for dead code analysis (see §Dead Code Analysis):
```rust
struct InferState {
    ctx:             InferenceContext,
    current_binding: Option<String>,               // which dict entry is currently being inferred
    use_def:         HashMap<String, HashSet<String>>, // per-binding use-def graph for liveness BFS
    // ... diagnostics, level, etc.
}
```

`unify(a: Arc<Value>, b: Arc<Value>, ctx: &mut InferenceContext)` — no Type conversion.

### `src/lower.rs`

**Current:** Sets `call_dispatch` on callsites when possible; synthesizes dispatch prefix strings; emits `TypeAssert { resolved_type: Type }`.

**Proposed:**
- Emits `TypeAssert { check: Resolved(arc_typevalue) }` from type checker output
- Emits MethodDispatcher tinct function for polymorphic method sites
- `call_dispatch` removed entirely

### `src/eval.rs` / `src/eval_materialize.rs`

**Current:** `call_dispatch` evaluated to choose instance; TypeAnnotationTable lookaside; DispatchObligation machinery.

**Proposed:**
- `CoreExpr::TypeAssert` evaluates by reading `value.type_val` and comparing to `check`
- MethodDispatcher is a plain tinct function — evaluation is identical to any function call
- `call_dispatch`, DispatchObligation, TypeAnnotationTable all removed

### `stdlib/builtin_core.llt`

**Current:** Declares builtins with `Any` in many positions; TypeNode shadow type system.

**Proposed:**
- Adds `TypeValue` and `RowTail` sum type declarations
- Adds the `repr:` table for all Rust-backed types
- All TypeNode.* declarations become redundant (eliminated)
- Builtin return types use specific TypeValues instead of `Any`

### `stdlib/prelude.llt`

**Current:** Numeric typeclasses partially in prelude; type predicates without `narrows:`.

**Proposed:**
- `Numeric`, `Integer`, `Float` marker typeclass declarations + instances
- Type predicates with `narrows:` annotations
- `Castable` typeclass and `cast` function
- MethodDispatcher emission for core polymorphic methods

## Prerequisites

- `Value::*` variants gaining `type_val` field — requires an atomic migration of all construction sites
- `repr_registry` in Rust that validates `repr:` strings at declaration time and resolves them at load time
- `InferenceContext` replacing `InferState` + `Substitution` — large refactor of type inference engine
- De Bruijn `TypeValue.Recursive` / `TypeValue.RecursiveRef` — eliminates gensym from equirecursive types
- `CoreExpr::TypeAssert` extended with `TypeAssertCheck::Resolved(Arc<Value>)` — lowerer must fill this from type checker output

## Relationship to Other Proposals

- **matchable-patterns.md** — depends on this. Pattern matching on TypeValues in match arms is specified here. `bind-primitive`/`bind-opaque` are replaced by `repr:`.
- **type-stage-foundation.md** — the long-term vision: type inference algorithms become tinct typeclass operations. This proposal does not require that; it is a prerequisite for it.
- **equirecursive-types.md** — de Bruijn `Recursive`/`RecursiveRef` specified here supersedes the gensym approach in equirecursive-types.md.
- **user-type-constructors.md** — `Type::TyCon` and `Type::App` variants are eliminated; replaced by `TypeValue.Op` and `TypeValue.App`.

## Prior Art

- **Gradual Typing** (Siek & Taha 2006; Garcia et al. 2016) — `TypeValue.Unknown` as `?`; consistency-based rather than subtype-based. The `Top` / `Unknown` distinction: Unknown = gradual escape hatch (AGT consistent), Top = lattice top (every type <: Top).
- **First-Class Types / Type Reification** (Reflect, Ur/Web) — types as first-class values that can be passed, stored, and inspected.
- **Dictionary-Passing Style** (Wadler & Blott 1989) — the MethodDispatcher implements dictionary-passing explicitly in tinct. The dispatch function IS the dictionary lookup.
- **Blame Calculus** (Wadler & Findler 2009) — `TypeAssert` carries a source span for blame tracking. `Castable`/`cast` as the conversion mechanism mirrors gradual typing's cast insertion.
- **Bidirectional Type Checking** (Dunfield & Krishnaswami 2021) — `TypeAssertCheck::Source` is the checking mode; `Resolved(arc)` is the synthesis mode.
- **Rémy Row Types** (Rémy 1989) — `RowTail` is a separate syntactic category distinct from TypeValue, per Rémy's original formulation. `RowTail.Var` + `InferenceContext.levels` mirrors row variable levels.

## References

- Garcia, R., Clark, A. M., & Tanter, É. (2016). Abstracting Gradual Typing. POPL 2016.
- Rémy, D. (1989). Type Checking Records and Variants in a Natural Extension of ML. POPL 1989.
- Siek, J. G., & Taha, W. (2006). Gradual Typing for Functional Languages. Scheme and Functional Programming Workshop 2006.
- Wadler, P., & Blott, S. (1989). How to Make Ad-Hoc Polymorphism Less Ad Hoc. POPL 1989.
- Wadler, P., & Findler, R. B. (2009). Well-Typed Programs Can't Be Blamed. ESOP 2009.
- Dunfield, J., & Krishnaswami, N. (2021). Bidirectional Typing. ACM Computing Surveys 54(5).
