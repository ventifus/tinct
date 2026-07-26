# 16b — The Rust-Tinct Protocol

This document defines the boundary between Rust and tinct: which concepts each side owns, how they name the same thing, and the rules that govern the interface. The axiom "Prelude speaks the Rust protocol" is only enforceable when the protocol is written down.

---

## 1. The Boundary

Rust and tinct are two languages that must agree on a small set of shared concepts. Rust owns the runtime — it allocates memory, runs functions, and drives evaluation. Tinct owns the program — it expresses computations, types, and data structures. The boundary is where these two worlds must agree.

The boundary has three layers:

1. **Values** — the runtime representations of tinct data in Rust memory
2. **Types** — the static type concepts shared between Rust's type system and tinct's type checker
3. **Protocol entries** — names that Rust unconditionally invokes in the prelude/stdlib

Each layer is defined once, in one place, with a single canonical mapping.

### Rust is agnostic to prelude naming

Rust has **zero knowledge** of prelude-specific identifiers. `Boolean`, `True`, `False`, `Seq`, `Cons`, `if`, `=`, `<` — none of these names exist in Rust. They are prelude conventions. A user may bring any prelude they choose, including one where all identifiers are in a different language: if they call their boolean type `布尔` with constructors `真`/`假`, Rust will not notice or care.

Prelude's responsibility is to adapt Rust's outputs to user-facing conventions. For example: comparison builtins return `Value::Int(0)` or `Value::Int(1)` — that is the Rust-side protocol. Prelude wraps these in user-facing Boolean variants (or whatever the prelude author chooses to call them). User code never sees the raw 0/1.

This is why `Value::Bool`, `Value::Seq`, and any other Rust type that encodes a prelude-specific concept are violations: they couple Rust to specific tinct naming that the user has no obligation to preserve.

---

## 2. Primitive Value Representations

Every tinct value is a Rust `Value` variant. For each primitive, the **tinct definition** is the exact declaration in `stdlib/builtin_core.llt` that establishes the type; the **Rust definition** is the exact `Value` variant in `src/value.rs`. Any deviation on either side breaks the contract.

Primitives have no `[type ...]` declaration in the runtime section of `builtin_core.llt` — they are declared in the **type-stage section** (`--- stage: "type"`) as constructors of the `TypeNode` ADT and exported as annotation names.

### Integer

**Tinct definition** (`stdlib/builtin_core.llt`, type-stage):
```tinct
TypeNode: [type ... Int ...]  # declares TypeNode.Int constructor
Integer: TypeNode.Int          # exports as @Integer annotation name
```

**Rust definition** (`src/value.rs`):
```rust
Value::Int(i64)  // 64-bit signed, two's complement
```

---

### Float

**Tinct definition**:
```tinct
TypeNode: [type ... Float ...]
Float: TypeNode.Float
```

**Rust definition**:
```rust
Value::Float(f64)  // IEEE 754 double precision
```

---

### String

**Tinct definition**:
```tinct
TypeNode: [type ... String ...]
String: TypeNode.String
```

**Rust definition** — zero-copy slice view into a shared backing buffer:
```rust
Value::String {
    source: Arc<str>,  // shared backing buffer (reference-counted)
    start:  usize,     // byte offset of string start (inclusive)
    end:    usize,     // byte offset of string end (exclusive)
}
```
Substrings share the same `source` with updated `start`/`end`. See R-24 for lifetime soundness analysis.

---

### Bytes

**Tinct definition**:
```tinct
TypeNode: [type ... Bytes ...]
Bytes: TypeNode.Bytes
```

**Rust definition** — same slice model as String:
```rust
Value::Bytes {
    source: Arc<[u8]>,
    start:  usize,
    end:    usize,
}
```

---

### Dict

**Tinct definition**:
```tinct
TypeNode: [type ... Dict: [fields: Any  open: Any  key-type: Any  value-type: Any] ...]
Dict: [TypeNode.Dict fields: []  open: 1  key-type: TypeNode.Top  value-type: TypeNode.Top]
```

The type-stage `Dict` value represents the open structural dict type (any keys, any values). The empty dict `[]` is a valid `Value::Dict` with zero entries and is the null/absent sentinel.

**Rust definition**:
```rust
Value::Dict(IndexMap<HashableValue, Arc<Thunk>>)  // ordered map, lazy values
```

---

### Function

#### Type definitions

Two function type concepts exist at the tinct level:

**`@Callable` — any callable (top of the function type lattice)**

```tinct
# In stdlib/builtin_core.llt type-stage:
TypeNode: [type ... Callable ...]

# In stdlib/prelude.llt type-stage (re-exported for user programs):
Callable: TypeNode.Callable
```

`@Callable` matches any function regardless of arity or signature. It is the escape hatch when the specific signature is unknown or irrelevant.

**`@[Fn@ReturnType [ParamType1 ParamType2 ...]]` — specific function signature**

This is not a declared type — it is a structural annotation form detected by the type checker. A two-entry positional dict where the first entry is `Fn@ReturnType` (a VarRef with return-type annotation) and the second is the parameter list resolves to a concrete `Type::Function`:

```tinct
f@[Fn@String [Integer Boolean]]        # function taking Integer and Boolean, returning String
g@[Fn@Boolean [x: Integer y: String]]  # named parameters
```

The `Fn` identifier here refers to the function type constructor — not a declared type, but a syntactic marker recognized by `try_resolve_fn_type_expr` in `src/typecheck_annot.rs`.

#### How to make one

User-defined functions use the `fn` special syntax (not a regular call — it is recognized by the parser):

```tinct
[fn [let x] body]                         # simplest form
[fn@String [let x@Integer] body]          # with return and parameter annotations
[fn@[return: String  doc: "..."] [let x@Integer] body]  # full metadata form
```

`fn` desugars to a `SurfaceExpression::Fn` node, which the evaluator converts to a `Value::Function` with closure conversion at the point of evaluation.

#### Rust definitions

User-defined function — closure-converted; all cross-scope captures resolved at creation time:
```rust
Value::Function {
    params:      Arc<Vec<Param>>,
    body:        Arc<Spanned<CoreExpr>>,
    closure_env: Arc<Vec<Arc<Thunk>>>,
    annotation:  Option<Box<FnAnnotation>>,
}
```

Builtin function — a Rust function pointer registered in `builtins_core.rs`; not user-constructable:
```rust
Value::Builtin(BuiltinDef)
```

---

### Nominal Variant

**Tinct definition** — every `[type ...]` declaration produces a variant type:
```tinct
Color: [type Red Green Blue]            # unit variants
Option: [type [let a] Some: [value: a] None]  # payload variant + unit variant
```

**Rust definition**:
```rust
Value::Variant {
    tycon:   Arc<str>,            // type name, e.g. "Color"
    ctor:    Arc<str>,            // constructor name, e.g. "Red"
    payload: Option<Arc<Thunk>>,  // None for unit variants
}
// Qualified tag: format!("{}.{}", tycon, ctor) — e.g. "Color.Red"
```

---

### Extended Numerics

**Tinct definition**:
```tinct
TypeNode: [type ... Decimal ... BigInt ...]
Decimal: TypeNode.Decimal
BigInt:  TypeNode.BigInt
```

**Rust definition**:
```rust
Value::Decimal(rust_decimal::Decimal)  // 96-bit software decimal
Value::BigInt(num_bigint::BigInt)      // arbitrary-precision integer
```

---

### Opaque Rust Types

These types have no tinct structural definition — Rust owns them entirely. Their tinct-side declaration is the unit constructor in the `TypeNode` ADT, which establishes only the opaque tag name:

```tinct
TypeNode: [type
  ...
  DirCap    # opaque — tag name only, no payload schema
  NetCap
  Handle
  File
  ...]

DirCap: TypeNode.DirCap   # exports as @DirCap annotation
```

**Rust definitions** — pattern matching on opaque values always fails:

| Tinct definition | Rust definition |
|---|---|
| `DirCap: TypeNode.DirCap` | `Value::DirCap { dir: cap_std::fs::Dir, perms: DirPerms }` |
| `NetCap: TypeNode.NetCap` | `Value::NetCap(Arc<Vec<NetCapEntry>>)` |
| `File: TypeNode.File` | `Value::File(Arc<Mutex<cap_std::fs::File>>)` |
| `Uri: TypeNode.Uri` | `Value::Uri { scheme: String, uri: String }` |
| `Handle: TypeNode.Handle` | `Arc`-wrapped handle type |
| `Task: TypeNode.Task` | `Arc`-wrapped async task |
| `Channel: TypeNode.Channel` | `Arc`-wrapped async channel |
| `Context: TypeNode.Context` | `Arc`-wrapped cancellation context |
| `ReactiveCell: TypeNode.ReactiveCell` | `Arc`-wrapped reactive cell |
| `ClockCap: TypeNode.ClockCap` | `Arc`-wrapped clock capability |
| `Timezone: TypeNode.Timezone` | `Arc`-wrapped timezone |
| `QuicSession: TypeNode.QuicSession` | `Arc`-wrapped QUIC session |
| `Http2Session: TypeNode.Http2Session` | `Arc`-wrapped HTTP/2 session |
| `Http3Session: TypeNode.Http3Session` | `Arc`-wrapped HTTP/3 session |
| `QuicDatagramHandle: TypeNode.QuicDatagramHandle` | `Arc`-wrapped QUIC datagram handle |
| `Program: TypeNode.Program` | Parsed program with resolutions and type info |
| `Document: TypeNode.Document` | Single document handle |
| `CoreDocument: TypeNode.CoreDocument` | Core document with entries and span |
| `TypeContext: TypeNode.TypeContext` | Type checker context |

Additionally, `Value::U64(u64)` (unsigned 64-bit, from `42u` literals), `Value::Timestamp(i64)`, `Value::Duration(i64)`, and `Value::RevocableDirCap` exist as Rust-internal variants not yet exported as protocol-level types. `Value::Builder`, `Value::Arena`, `Value::Annotated`, `Value::BroadcastChannel`, `Value::OneshotSender`, `Value::OneshotReceiver`, and `Value::Expression` are infrastructure variants used internally by the evaluator.

### String and Bytes lifetime

`Arc<str>` and `Arc<[u8]>` manage backing-buffer lifetime via reference counting. When Rust code creates a new tinct string, it allocates a fresh `Arc<str>` at `start=0, end=len`. Substrings share the `Arc` with updated `start`/`end` — zero copy. The backing buffer lives until the last `Value::String` or `Value::Bytes` referencing it is dropped. See R-24 for lifetime soundness analysis.

---

## 3. Primitive Type Concepts

The type checker operates on `Type` variants (Rust enum). The tinct type-stage represents these as `TypeNode` constructors declared in `stdlib/builtin_core.llt`. The canonical mapping is `typenode_leaf_to_type` in `src/type_normalize.rs` — **the single authoritative function for this mapping; no other Rust code may hardcode TypeNode names**.

The three-way binding below defines the contract. Any deviation — in the annotation name, the TypeNode constructor, or the Rust variant — breaks it.

| Tinct annotation | `builtin_core.llt` type-stage declaration | Rust `Type` variant |
|---|---|---|
| `@Integer` | `Integer: TypeNode.Int` | `Type::Int` |
| `@Float` | `Float: TypeNode.Float` | `Type::Float` |
| `@String` | `String: TypeNode.String` | `Type::Str` |
| `@Bytes` | `Bytes: TypeNode.Bytes` | `Type::Bytes` |
| `@Any` | `Any: TypeNode.Top` | `Type::Any` |
| `@Unknown` | `Unknown: TypeNode.Unknown` | `Type::Unknown` |
| `@Never` | `Never: TypeNode.Never` | `Type::Never` |
| `@Proxy` | `Proxy: TypeNode.Proxy` | `Type::Proxy` |
| `@Callable` | `builtin_core.llt` declares `TypeNode.Callable` constructor; `prelude.llt` exports `Callable: TypeNode.Callable` | `Type::Function { params:[], rest:Some(..), ret:Any, .. }` |
| `@Dict` | `Dict: [TypeNode.Dict fields:[] open:1 ..]` | `Type::Dict(open row)` |

**Opaque types** — map to `Type::TyCon(name)`, matched only by tag. Tinct definition is the annotation name; Rust definition is the `Value` variant.

| Tinct annotation | `builtin_core.llt` declaration | Rust `Type` variant |
|---|---|---|
| `@DirCap` | `DirCap: TypeNode.DirCap` | `Type::TyCon("DirCap")` |
| `@NetCap` | `NetCap: TypeNode.NetCap` | `Type::TyCon("NetCap")` |
| `@Handle` | `Handle: TypeNode.Handle` | `Type::TyCon("Handle")` |
| `@File` | `File: TypeNode.File` | `Type::TyCon("File")` |
| `@BuilderHandle` | `BuilderHandle: TypeNode.BuilderHandle` | `Type::TyCon("BuilderHandle")` |
| `@Task` | `Task: TypeNode.Task` | `Type::TyCon("Task")` |
| `@Channel` | `Channel: TypeNode.Channel` | `Type::TyCon("Channel")` |
| `@Context` | `Context: TypeNode.Context` | `Type::TyCon("Context")` |
| `@ReactiveCell` | `ReactiveCell: TypeNode.ReactiveCell` | `Type::TyCon("ReactiveCell")` |
| `@ClockCap` | `ClockCap: TypeNode.ClockCap` | `Type::TyCon("ClockCap")` |
| `@Timezone` | `Timezone: TypeNode.Timezone` | `Type::TyCon("Timezone")` |
| `@Decimal` | `Decimal: TypeNode.Decimal` | `Type::TyCon("Decimal")` |
| `@BigInt` | `BigInt: TypeNode.BigInt` | `Type::TyCon("BigInt")` |
| `@Program` | `Program: TypeNode.Program` | `Type::TyCon("Program")` |
| `@Document` | `Document: TypeNode.Document` | `Type::TyCon("Document")` |
| `@CoreDocument` | `CoreDocument: TypeNode.CoreDocument` | `Type::TyCon("CoreDocument")` |
| `@TypeContext` | `TypeContext: TypeNode.TypeContext` | `Type::TyCon("TypeContext")` |
| `@QuicSession` | `QuicSession: TypeNode.QuicSession` | `Type::TyCon("QuicSession")` |
| `@QuicDatagramHandle` | `QuicDatagramHandle: TypeNode.QuicDatagramHandle` | `Type::TyCon("QuicDatagramHandle")` |
| `@Http2Session` | `Http2Session: TypeNode.Http2Session` | `Type::TyCon("Http2Session")` |
| `@Http3Session` | `Http3Session: TypeNode.Http3Session` | `Type::TyCon("Http3Session")` |
| `@Uri` | `Uri: TypeNode.Uri` | `Type::TyCon("Uri")` |
| `@Urn` | `Urn: TypeNode.Urn` | `Type::TyCon("Urn")` |

### The canonical mapping function

`typenode_leaf_to_type(val: &Value) -> Option<Type>` in `src/type_normalize.rs` is the **only** Rust code that should translate TypeNode variant values to `Type` variants. If a new primitive type is added:
1. Add a `Value` variant in `src/value.rs`
2. Add a `Type` variant in `src/type_def.rs`
3. Declare a `TypeNode` constructor in `stdlib/builtin_core.llt`
4. Add an arm to `typenode_leaf_to_type` in `src/type_normalize.rs`

These four changes must happen together. Nothing else in Rust should know TypeNode names.

---

## 4. Who Owns What

**Rust owns:**
- The primitive value representations (`Value` variants)
- The primitive type concepts (`Type` variants)
- The evaluator, memory management, and built-in function implementations
- The canonical mapping: `typenode_leaf_to_type`
- The list of opaque Rust types

**Tinct (prelude and user code) owns:**
- The names by which users refer to primitive types (`Integer`, `String`, `DirCap`, etc.) — any prelude can use different names
- All user-defined types: `Boolean`, `Seq`, `Option`, `Result`, and every other nominal ADT — these are prelude/user constructs, not Rust primitives
- The composition operators for building complex types (`union`, `or`, `all`, type aliases)
- The `TypeNode` ADT declaration (in `builtin_core.llt`) — the tinct-side representation of type concepts
- All type class definitions and instances
- All standard library functions

**Explicitly NOT in Rust:**
`Boolean`, `Seq`, `True`, `False`, `if`, `=`, `<`, `>`, and every other prelude-defined identifier. Rust has no code that references these names. They are user-land conventions. A custom prelude may name them anything.

**The protocol (what both sides agree on):**
- `builtin_core.llt` is the tinct-side declaration of all Rust-owned concepts. It is the contract. Any name in `builtin_core.llt`'s type-stage section that maps to a `TypeNode` constructor is part of the protocol.
- Rust never invents new names for tinct-side concepts. Tinct never invents new names for Rust-side concepts without a corresponding `builtin_core.llt` declaration.
- Rust code that hardcodes any prelude-specific identifier string (e.g. `"Boolean"`, `"True"`, `"Seq"`) is a violation of this protocol.

---

## 5. The Builtin Function Interface

Rust implements a set of primitive functions — builtins — that are not expressible in tinct itself (arithmetic, memory allocation, I/O, etc.). Every builtin has a tinct-side declaration in one of the `stdlib/builtin_*.llt` files. These declarations form the tinct half of the builtin interface.

### Naming convention

Builtins use the `builtin-` prefix. This prefix is the contract: it tells callers they are calling a Rust-implemented primitive, not a tinct function. No prelude or user function should use this prefix — it is reserved for the Rust-tinct boundary.

```
builtin-int-add       builtin-str-length     builtin-file-open
builtin-dict-nth      builtin-make-builder   builtin-lower
```

### Declaration format

Every builtin is declared in a `builtin_*.llt` file with the actual function body replaced by `...` (a placeholder that errors if evaluated — builtins are Rust-side):

```tinct
builtin-int-add:   [fn@Integer [let a@Integer b@Integer] ...]
builtin-eq-int:    [fn@[or 0 1] [let a@Integer b@Integer] ...]
builtin-dict-nth:  [fn@Any [let d@Dict n@Integer] ...]
builtin-raise:     [fn@Never [let msg@String] ...]
```

The return type and parameter types use only the primitive types defined in this protocol document. No prelude-specific type names appear in builtin signatures — a custom prelude can use different names for Boolean, Seq, etc. and builtins remain agnostic.

### Integer return types and range annotations

**Rust side**: Rust builtins that return a yes/no result return `Value::Int(0)` or `Value::Int(1)` — a plain integer. Rust produces an integer value; that is all.

**Tinct declaration side**: The `builtin_*.llt` declaration annotates the return type as `@[or 0 1]`. This is a tinct type annotation — it documents the precise range of values that Rust will produce, making it visible to the tinct type checker. With this annotation, the type checker knows that matching on `0:` and `1:` provides exhaustive coverage (no `...:` wildcard is needed):

```tinct
builtin-eq-int: [fn@[or 0 1] [let a@Integer b@Integer] ...]
```

The `[or 0 1]` is a tinct-side description of what the Rust integer values can be. Rust does not "return a union type" — it returns an integer with a well-defined range. The annotation exists so tinct programs can pattern-match cleanly and the type checker can verify exhaustiveness.

What these 0/1 values mean to user programs is entirely prelude's responsibility. Prelude may wrap them in user-defined variants, pass them directly to `[match c 0: ... 1: ...]`, or use them however fits the program. Rust has no opinion.

### File organization

| File | Contents |
|---|---|
| `stdlib/builtin_core.llt` | Core primitives: arithmetic, comparison, dict ops, builder, string, type system, resolve, eval |
| `stdlib/builtin_string.llt` | Extended string operations |
| `stdlib/builtin_math.llt` | Math operations (floor, round, etc.) |
| `stdlib/builtin_io.llt` | I/O: file, directory, network primitives |
| `stdlib/builtin_async.llt` | Async primitives: task, channel, context, timeout |
| `stdlib/builtin_meta.llt` | Meta-evaluation: parse, desugar, typecheck, lower, eval |

### The invariant

Every function registered in `src/builtins_core.rs` (and the other `builtins_*.rs` files) must have a corresponding declaration in a `builtin_*.llt` file. The declaration is the tinct-side half of the interface — without it, the type checker cannot see the builtin, and user code cannot call it with type safety.

### Builtins are only allowed when impossible in tinct

A function may be a builtin **only if it is impossible to implement correctly in tinct using the existing primitive types**. This is a hard constraint, not a preference. If a function can be expressed in tinct — even with more verbose code — it must be expressed in tinct and must not be a builtin.

Furthermore, the parameter types and return types of any builtin declaration must use **only the primitive types defined in this document** (Integer, Float, String, Bytes, Dict, Variant, Callable, the opaque types, and `[or 0 1]` for boolean results). A builtin signature that references a prelude-defined type name would couple Rust to that prelude's naming conventions — violating the boundary.

This principle is not aspirational. Every builtin that exists today should be scrutinized: can it be removed and replaced with a tinct implementation over a narrower set of truly irreducible primitives? The comment in `builtin_core.llt` records past reductions:

```
# builtin-merge: implemented in tinct prelude and loader/test-loader (using builder API).
# builtin-append: implemented in tinct prelude (derived from builtin-merge).
```

`merge` now uses `builtin-make-builder` / `builtin-builder-set` / `builtin-builder-finish` and is expressed entirely in tinct. The builder API represents the irreducible primitive; `merge` is not. This is the correct direction of travel.

---

## 6. AST Types and the Introspection Schema

AST types are a deliberate exception to the rule that Rust is agnostic to tinct naming. When tinct programs inspect or construct code at compile time (macros, the formatter, quasiquoting), they work with AST nodes that Rust serializes into tinct dicts and variants. Both sides must agree exactly on the structure. This is a **versioned schema coupling**, documented in `doc/feature/ast-schema.md`.

### How AST values are represented

AST nodes use the standard tinct primitive types — `Value::Variant` and `Value::Dict` — rather than new Rust-specific Value variants. There is no `Value::Ast` or `Value::Expr`. All AST expression nodes have `tycon: "Expr"` and a `ctor` naming the node type. In tinct, they are constructed and matched exactly like any other nominal variant:

```tinct
# A function call, as a tinct value — constructing it:
[Expr.Call
  fn:         [Expr.VarRef name: "map"  span: [start: [line: 1  col: 1] end: [line: 1  col: 4]]]
  args:       [[Expr.VarRef name: "f"  span: [...]]]
  named-args: []
  implied:    1
  span:       [...]]

# Pattern-matching on AST nodes:
[match node
  [case [let p] [Expr.VarRef p]    p.name]
  [case [let p] [Expr.Call p]      p.fn]
  [case [let p] [Expr.Literal p]   p.value]
  ...:                             []]
```

Rust serializes the AST into this form in `src/surface_convert.rs`. Tinct code that processes AST nodes uses ordinary tinct operations — pattern matching, field access, dict construction.

### The schema is the contract

The Variant tag names (`"Call"`, `"VarRef"`, `"Literal"`, `"DotAccess"`, etc.) and the field names within each node (`fn:`, `args:`, `name:`, `span:`, `kind:`, `value:`) are the shared schema. Both Rust and tinct must use these names exactly. A change to either side without updating the other breaks macro code and the formatter.

The schema is versioned: `schema-version: 1` appears on the root `File` node. Breaking schema changes require a version bump.

### Why this coupling is legitimate

The general principle — Rust is agnostic to prelude naming — holds for all runtime values. AST introspection is different: the AST is inherently about the program text, which is Rust's domain. Rust defines the grammar and therefore defines the AST structure. Tinct code that processes AST nodes is necessarily coupled to the grammar. This coupling is:

- **Explicit** — documented in `doc/feature/ast-schema.md` as a canonical schema
- **Versioned** — schema changes are tracked
- **Minimal** — AST nodes use Dict and Variant, not new Value types
- **Necessary** — there is no way to express "this field contains the callee expression of a function call" without agreeing on a name

### Opaque AST handles

`Program`, `Document`, and `CoreDocument` in the opaque types table (§2) are handles to unevaluated AST objects. They are passed to builtins like `builtin-program-docs`, `builtin-doc-meta`, `builtin-resolve`, `builtin-lower` to drive the pipeline. The actual AST *content* — expressions, entries, spans — is exposed as dicts and variants per the schema, not as opaque handles.

---

## 7. Protocol Entry Points

These are names that Rust unconditionally references by string in its source — they are part of the protocol and any custom prelude must provide them. Omitting any causes a runtime or resolution error for the associated feature.

| Name | Hardcoded in | Purpose |
|---|---|---|
| `tmpl` | `src/desugar.rs` | Called for every interpolated string literal (`i"..."`) |
| `unindent` | `src/desugar.rs` | Called for every triple-quoted string literal (`"""..."""`) |
| `as-typenode` | `src/typecheck_annot.rs` | Converts composite type expressions to TypeNode values during annotation resolution |
| `builtin-get` | `src/resolve.rs` | The resolver pre-resolves `builtin-get` for field access desugaring (`.x` syntax lowers to `[builtin-get "x" val]`) |
| `builtin-make-annotated` | `src/lower.rs` | Wraps variant/constructor values with annotation metadata dict |
| `to-match` | `src/eval.rs` | Match signal dispatch — called when pattern matching needs to test whether a value matches a given arm. Injected by class declaration lowering. |
| `Fn` | `src/typecheck_annot.rs` | Function type annotation syntax: `@[Fn@ReturnType [Params]]` — the `Fn` identifier in bracket-head position is detected structurally to produce a concrete `Type::Function`. Any prelude that provides the function type annotation form must use this name. |
| `Seq`, `Cons`, `End`, `head`, `tail` | `src/eval_call.rs` | Typed variadic parameter binding. When a function has `[fn [let ...args@Type] ...]`, Rust assembles matched args into a `Seq.Cons`/`Seq.End` cons-list. Any prelude must declare `Seq: [type [let a] Cons: [head: a  tail: [Seq a]] End]` with `head` and `tail` as the payload field names for this feature to work. |
| `RecursiveRef`, `Union`, `Intersect`, `types` | `src/builtins_meta.rs` (`is_contractive_value`) | Contractiveness check for recursive types (`builtin-is-contractive`). A `Value::Variant` whose `ctor` field is `"RecursiveRef"` is non-contractive (bare recursive reference). Variants with `ctor` `"Union"` or `"Intersect"` are non-guarding combinators: their contractiveness is determined by recursing into every element of their `types` payload field (an integer-keyed Dict of child TypeNode values). Any conforming prelude `TypeNode` declaration must use these exact constructor names and the `types` field name for the recursive-type contractiveness feature to work correctly. |
| `do-infer-placeholder: 1` | `src/surface_convert.rs` (`dict_to_surface_node_inner`) | Do-notation inferred monad sentinel. When prelude's do-desugar produces an `Expr.VarRef` for the inferred monad placeholder, it must include `do-infer-placeholder: 1` (an integer `1`) in the `Expr.VarRef` payload dict. `surface_convert.rs` reads this field and sets `SurfaceExpression::VarRef::do_infer_placeholder = true` on the resulting AST node. The type checker (`src/typecheck_cek.rs`) returns `Type::Unknown` for any call whose function head is a Field node whose target is such a VarRef, deferring monad-type resolution to the evaluator. Any custom prelude implementing do-notation with inferred monads must emit this field; omitting it will cause the type checker to attempt full inference on an unresolvable call. |

| `each` | `src/typecheck_annot.rs` | Constraint annotation list syntax: `@[constraint: [each Cls1 Cls2]]` — the `each` identifier in bracket-head position in a constraint list is detected to expand multiple class names. Analogous to `Fn` for function type annotations. |
| `=`, `and`, `has?`, `type-of` | `src/typecheck_narrow.rs` | Structural narrowing protocol entries (D-8). Rust dispatches path-sensitive type narrowing when it detects these specific function names as the condition in a guard expression. Documented in `doc/feature/narrowing.md §Structural Narrowing Protocol Entries`. |

Note: `class == "Indexable"` dispatch in `src/type_unify.rs` and `"get"`/`"get?"`/`"get-in"` name dispatch in `src/typecheck_cek.rs` are **known Axiom 1 violations** blocked on S-992, which will implement general annotation-driven dispatch. `if` must have no special Rust handling — it is a plain tinct function defined in prelude.

---

## 8. Adding a New Primitive Type

Checklist:
1. `src/value.rs` — add `Value` variant with Rust representation
2. `src/type_def.rs` — add `Type` variant (or use `Type::TyCon("Name")` for opaque types)
3. `src/type_normalize.rs` — add arm to `typenode_leaf_to_type`
4. `stdlib/builtin_core.llt` — add constructor to `TypeNode: [type ...]` and export name (`MyType: TypeNode.MyType`)
5. `src/imports.rs` — if opaque: the auto-derivation pass in `build_builtin_core_envs_inner` will pick it up automatically from the type-stage scope
6. `src/eval.rs` — add `value_matches_type` dispatch arm if runtime type-checking is needed

Steps 1–4 are mandatory for every new primitive. Steps 5–6 apply only to opaque Rust types.
