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

This is not a declared type — it is a structural annotation form detected by the type checker. A two-entry positional dict where the first entry is `Fn@ReturnType` (a VarRef with return-type annotation) and the second is the parameter list resolves to a concrete `TypeValue.Fn`:

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

Additionally, `Value::U64(u64)` (unsigned 64-bit, from `42u` literals), `Value::Timestamp(jiff::Timestamp)`, `Value::Duration(i64)`, and `Value::RevocableDirCap` exist as Rust-internal variants not yet exported as protocol-level types. `Value::Builder`, `Value::Arena`, `Value::Annotated`, `Value::BroadcastChannel`, `Value::OneshotSender`, `Value::OneshotReceiver`, and `Value::Expression` are infrastructure variants used internally by the evaluator.

### String and Bytes lifetime

`Arc<str>` and `Arc<[u8]>` manage backing-buffer lifetime via reference counting. When Rust code creates a new tinct string, it allocates a fresh `Arc<str>` at `start=0, end=len`. Substrings share the `Arc` with updated `start`/`end` — zero copy. The backing buffer lives until the last `Value::String` or `Value::Bytes` referencing it is dropped. See R-24 for lifetime soundness analysis.

---

## 3. Primitive Type Concepts

The type checker operates on `TypeValue` values (`Arc<Value>` with `TypeValue.*` constructor tags). The tinct type-stage represents these as `TypeNode` constructors declared in `stdlib/builtin_core.llt`. The canonical mappings are `typenode_value_to_type` (TypeNode → TypeValue, async, in `src/typecheck_annot.rs`), `typevalue_to_typenode` (TypeValue → TypeNode, in `src/type_class.rs`), and `typenode_ctor_to_typevalue` (unit TypeNode bare name → TypeValue, synchronous, in `src/type_infer.rs`) — **the three authorized translators; no other Rust code may hardcode TypeNode names**.

The three-way binding below defines the contract. Any deviation — in the annotation name, the TypeNode constructor, or the Rust variant — breaks it.

| Tinct annotation | `builtin_core.llt` type-stage declaration | Rust `TypeValue` constructor |
|---|---|---|
| `@Integer` | `Integer: TypeNode.Int` | `TypeValue.Repr("Value::Int")` |
| `@Float` | `Float: TypeNode.Float` | `TypeValue.Repr("Value::Float")` |
| `@String` | `String: TypeNode.String` | `TypeValue.Repr("Value::String")` |
| `@Bytes` | `Bytes: TypeNode.Bytes` | `TypeValue.Repr("Value::Bytes")` |
| `@Any` | `Any: TypeNode.Top` | `TypeValue.Top` |
| `@Unknown` | `Unknown: TypeNode.Unknown` | `TypeValue.Unknown` |
| `@Never` | `Never: TypeNode.Never` | `TypeValue.Never` |
| `@Absent` | `Absent: TypeNode.Absent` | `TypeValue.Record { fields: {}, tail: closed }` — the empty closed record; the static type of the null/absent sentinel `[]` |
| `@Proxy` | `Proxy: TypeNode.Proxy` | `TypeValue.Repr("Value::Proxy")` |
| `@Callable` | `builtin_core.llt` declares `TypeNode.Callable` constructor; `prelude.llt` exports `Callable: TypeNode.Callable` | `TypeValue.Fn { params: [], variadic: true, return: Unknown }` |
| `@Dict` | `Dict: [TypeNode.Dict fields:[] open:1 ..]` | `TypeValue.Record { fields: {}, tail: Uniform(Top) }` — open record |

**Opaque types** — map to `TypeValue.Op(name)`, matched only by tag. Tinct definition is the annotation name; Rust definition is the `Value` variant.

| Tinct annotation | `builtin_core.llt` declaration | Rust `TypeValue` constructor |
|---|---|---|
| `@DirCap` | `DirCap: TypeNode.DirCap` | `TypeValue.Op("DirCap")` |
| `@NetCap` | `NetCap: TypeNode.NetCap` | `TypeValue.Op("NetCap")` |
| `@Handle` | `Handle: TypeNode.Handle` | `TypeValue.Op("Handle")` |
| `@File` | `File: TypeNode.File` | `TypeValue.Op("File")` |
| `@BuilderHandle` | `BuilderHandle: TypeNode.BuilderHandle` | `TypeValue.Op("BuilderHandle")` |
| `@Task` | `Task: TypeNode.Task` | `TypeValue.Op("Task")` |
| `@Channel` | `Channel: TypeNode.Channel` | `TypeValue.Op("Channel")` |
| `@Context` | `Context: TypeNode.Context` | `TypeValue.Op("Context")` |
| `@ReactiveCell` | `ReactiveCell: TypeNode.ReactiveCell` | `TypeValue.Op("ReactiveCell")` |
| `@ClockCap` | `ClockCap: TypeNode.ClockCap` | `TypeValue.Op("ClockCap")` |
| `@Timezone` | `Timezone: TypeNode.Timezone` | `TypeValue.Op("Timezone")` |
| `@Decimal` | `Decimal: TypeNode.Decimal` | `TypeValue.Op("Decimal")` |
| `@BigInt` | `BigInt: TypeNode.BigInt` | `TypeValue.Op("BigInt")` |
| `@Program` | `Program: TypeNode.Program` | `TypeValue.Op("Program")` |
| `@Document` | `Document: TypeNode.Document` | `TypeValue.Op("Document")` |
| `@CoreDocument` | `CoreDocument: TypeNode.CoreDocument` | `TypeValue.Op("CoreDocument")` |
| `@TypeContext` | `TypeContext: TypeNode.TypeContext` | `TypeValue.Op("TypeContext")` |
| `@QuicSession` | `QuicSession: TypeNode.QuicSession` | `TypeValue.Op("QuicSession")` |
| `@QuicDatagramHandle` | `QuicDatagramHandle: TypeNode.QuicDatagramHandle` | `TypeValue.Op("QuicDatagramHandle")` |
| `@Http2Session` | `Http2Session: TypeNode.Http2Session` | `TypeValue.Op("Http2Session")` |
| `@Http3Session` | `Http3Session: TypeNode.Http3Session` | `TypeValue.Op("Http3Session")` |
| `@Uri` | `Uri: TypeNode.Uri` | `TypeValue.Op("Uri")` |
| `@Urn` | `Urn: TypeNode.Urn` | `TypeValue.Op("Urn")` |

### The canonical mapping functions

Three functions are the authorized translators between TypeNode values and TypeValue:

- `typenode_value_to_type(val: &Value, ...) -> Result<Option<TypeValue>>` in `src/typecheck_annot.rs` — the **full forward mapping** (async): TypeNode → TypeValue. Handles all TypeNode constructors: leaf (no-payload or opaque) constructors (`Int`, `String`, `Float`, `Bytes`, `Never`, `Top`, `Unknown`, `Absent`, `Proxy`, `Callable`, and all opaque Rust types) as well as complex (payload-carrying) constructors: `Dict`, `TypeConstructor`, `TypeApplication`, `TypeVar`, `IntLiteral`, `StringLiteral`, `Union`, `Intersect`, `Negation`, `Arrow`, `Recursive`, and `RecursiveRef`. Each arm reads specific payload field names that are part of the protocol (documented in §7). Requires an async eval context and a type-stage scope.

- `typevalue_to_typenode(tv: &TypeValue, ...) -> Option<TypeValue>` in `src/type_class.rs` — the **inverse mapping**: TypeValue → TypeNode. Used when the type inference engine needs to pass a TypeValue back into the tinct type-stage as a TypeNode value (e.g., for resolver functional dependency calls). Constructs TypeNode Variant values using the same `TN_*` field constants as `typenode_value_to_type`. This function is the authorized inverse; no other Rust code may construct TypeNode Variant values from TypeValues.

- `typenode_ctor_to_typevalue(bare_ctor: &str) -> Option<TypeValue>` in `src/type_infer.rs` — the **synchronous unit-constructor mapping**: bare TypeNode constructor name → TypeValue. Covers only the unit (no-payload) primitive constructors that can appear as pin values in match patterns: `Int`, `Float`, `String`, `Bytes`, `Proxy`, `Callable`. Returns `None` for payload-carrying or abstract constructors, signalling the caller to fall through to value equality. This function is pure and requires no context; it delegates to the same `make_typevalue_*` calls as `typenode_value_to_type`. Use this when async context is unavailable (e.g., pattern matching in the evaluator).

No other Rust code may hardcode TypeNode constructor names outside these three functions.

If a new primitive type is added:
1. Add a `Value` variant in `src/value.rs`
2. Declare a `TypeNode` constructor in `stdlib/builtin_core.llt`
3. Add an arm to `typenode_value_to_type` in `src/typecheck_annot.rs`
4. If the new constructor is a unit primitive (no payload), also add an arm to `typenode_ctor_to_typevalue` in `src/type_infer.rs`

These changes must happen together. Nothing else in Rust should know TypeNode names.

---

## 4. Who Owns What

**Rust owns:**
- The primitive value representations (`Value` variants)
- The primitive type concepts (TypeValue constructors)
- The evaluator, memory management, and built-in function implementations
- The canonical mapping: `typenode_value_to_type`
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

The parameter and return types use only the primitive types defined in this protocol document — no prelude-specific type names appear in builtin signatures. A custom prelude can use different names for Boolean, Seq, etc. and builtins remain agnostic.

**Builtins may return any composition of primitive types.** Returning a structured dict such as `[fn@[program: Program  diagnostics: Dict] ...]` is correct and encouraged — that dict is built from primitives (`Program` is an opaque primitive, `Dict` is a primitive). The field names (`program:`, `diagnostics:`) are part of the builtin's protocol definition, documented in `builtin_core.llt` and available to all code that calls the builtin.

**`Any` in type annotations is strongly discouraged.** A return type of `Any` means the type checker cannot verify anything about what the builtin produces, which weakens all downstream type checking. Use `Any` only when Rust genuinely cannot know the type — for example, `builtin-dict-get` returns `Any` because the type of a dict value at a given key is unknown at compile time. When the type IS known (a specific opaque type, a specific structural dict, a specific integer range), it must be stated precisely. Prefer `Unknown` over `Any` for gradual-typing escape hatches — `Unknown` signals "we don't know" rather than "anything goes".

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
| `input-ast` | `src/formatter.rs` (`FORMATTER_INPUT_VAR`) | The variable name that formatter scripts use to receive the parsed AST dict. `resolve_surface_document_with_seed_frames` seeds the slot assignment for `input-ast` (establishing its name-to-slot mapping in the resolver), and `eval_surface_file_with_input` subsequently injects the actual AST thunk at that slot. Any custom formatter script must bind its AST input to `input-ast`; any other name produces a resolution error. The canonical string is exported as `tinct::formatter::FORMATTER_INPUT_VAR`. |
| `tmpl` | `src/desugar.rs` | Called for every interpolated string literal (`i"..."`) |
| `unindent` | `src/desugar.rs` | Called for every triple-quoted string literal (`"""..."""`) |
| `as-typenode` | `src/typecheck_annot.rs` | Converts composite type expressions to TypeNode values during annotation resolution |
| `builtin-dict-get` | `src/resolve.rs` | The resolver pre-resolves `builtin-dict-get` for field access desugaring (`.x` syntax lowers to `[builtin-dict-get "x" val]`) |
| `builtin-make-annotated` | `src/lower.rs` | Wraps variant/constructor values with annotation metadata dict |
| `to-match` | `src/eval.rs` | Match signal dispatch — called when pattern matching needs to test whether a value matches a given arm. Injected by class declaration lowering. |
| `Fn` | `src/typecheck_annot.rs` | Function type annotation syntax: `@[Fn@ReturnType [Params]]` — the `Fn` identifier in bracket-head position is detected structurally to produce a concrete `TypeValue.Fn`. Any prelude that provides the function type annotation form must use this name. |
| `RecursiveRef`, `Union`, `Intersect`, `types` | `src/builtins_meta.rs` (`is_contractive_value`) | Contractiveness check for recursive types (`builtin-is-contractive`). A `Value::Variant` whose `ctor` field is `"RecursiveRef"` is non-contractive (bare recursive reference). Variants with `ctor` `"Union"` or `"Intersect"` are non-guarding combinators: their contractiveness is determined by recursing into every element of their `types` payload field (an integer-keyed Dict of child TypeNode values). Any conforming prelude `TypeNode` declaration must use these exact constructor names and the `types` field name for the recursive-type contractiveness feature to work correctly. |
| `do-infer-placeholder: 1` | `src/surface_convert.rs` (`dict_to_surface_node_inner`) | Do-notation inferred monad sentinel. When prelude's do-desugar produces an `Expr.VarRef` for the inferred monad placeholder, it must include `do-infer-placeholder: 1` (an integer `1`) in the `Expr.VarRef` payload dict. `surface_convert.rs` reads this field and sets `SurfaceExpression::VarRef::do_infer_placeholder = true` on the resulting AST node. The type checker (`src/typecheck_cek.rs`) returns `Type::Unknown` for any call whose function head is a Field node whose target is such a VarRef, deferring monad-type resolution to the evaluator. Any custom prelude implementing do-notation with inferred monads must emit this field; omitting it will cause the type checker to attempt full inference on an unresolvable call. |

| `each` | `src/typecheck_annot.rs` | Constraint annotation list syntax: `@[constraint: [each Cls1 Cls2]]` — the `each` identifier in bracket-head position in a constraint list is detected to expand multiple class names. Analogous to `Fn` for function type annotations. |
| `=`, `and`, `has?`, `type-of` | `src/typecheck_narrow.rs` | Structural narrowing protocol entries (D-8). Rust dispatches path-sensitive type narrowing when it detects these specific function names as the condition in a guard expression. Documented in `doc/feature/narrowing.md §Structural Narrowing Protocol Entries`. |
| `IntLiteral` (payload field `n: Int`), `StringLiteral` (payload field `s: String`) | `src/type_normalize.rs` (`type_to_typenode`), `src/typecheck_annot.rs` (`typenode_value_to_type`) | TypeNode constructors for integer and string literal types. `type_to_typenode` produces `Value::Variant { tycon: "TypeNode", ctor: "IntLiteral", payload: Dict { "n": i64 } }` and `Value::Variant { tycon: "TypeNode", ctor: "StringLiteral", payload: Dict { "s": String } }`. `typenode_value_to_type` reads the `"n"` field from an `IntLiteral` payload and the `"s"` field from a `StringLiteral` payload to produce `TypeValue.IntLit { value: Int }` / `TypeValue.StrLit { value: String }`. A conforming custom prelude must declare `TypeNode.IntLiteral` with payload field `n: Int` and `TypeNode.StringLiteral` with payload field `s: String` for literal integer and string types to work correctly. |
| `Union` (payload field `types: Dict`), `Intersect` (payload field `types: Dict`) | `src/type_normalize.rs` (`type_to_typenode`), `src/typecheck_annot.rs` (`typenode_value_to_type`), `src/builtins_meta.rs` (`is_contractive_value`) | TypeNode constructors for union and intersection types. `type_to_typenode` produces `Value::Variant { tycon: "TypeNode", ctor: "Union"/"Intersect", payload: Dict { "types": integer-keyed Dict of child TypeNode values } }`. `typenode_value_to_type` reads the `"types"` field (an integer-keyed dict of child TypeNode values) to produce the corresponding `TypeValue.Union { members: Dict }` / `TypeValue.Inter { members: Dict }`. `is_contractive_value` recurses into the `"types"` field to check contractiveness of recursive types. A conforming custom prelude must declare `TypeNode.Union` and `TypeNode.Intersect` each with payload field `types: Any` (an integer-keyed dict of child TypeNode values) for union/intersection types and recursive-type contractiveness checking to work correctly. |
| `Negation` (payload field `inner: Any`) | `src/typecheck_annot.rs` (`typenode_value_to_type`) | TypeNode constructor for negation types. `typenode_value_to_type` reads the `"inner"` field (a single child TypeNode value) from a `Negation` payload to produce `TypeValue.Neg { of: TypeValue }`. A conforming custom prelude must declare `TypeNode.Negation` with payload field `inner: Any` for negation type annotations to work correctly. |
| `Arrow` (payload fields `params: Any`, `result: Any`) | `src/typecheck_annot.rs` (`typenode_value_to_type`) | TypeNode constructor for arrow (function) types. `typenode_value_to_type` reads the `"params"` field (an integer-keyed dict of parameter TypeNode values) and `"result"` field (the return TypeNode value) from an `Arrow` payload to produce `TypeValue.Fn`. A conforming custom prelude must declare `TypeNode.Arrow` with payload fields `params: Any` and `result: Any` for arrow type annotations to work correctly. |
| `Recursive` (payload field `body: Any`) | `src/typecheck_annot.rs` (`typenode_value_to_type`) | TypeNode constructor for recursive (mu) types. `typenode_value_to_type` reads the `"body"` field (the body TypeNode value) from a `Recursive` payload to produce `TypeValue.Recursive { body: TypeValue }`. The de Bruijn representation has no bound variable name — there is no `var` field. A conforming custom prelude must declare `TypeNode.Recursive` with payload field `body: Any` for recursive type annotations to work correctly. |
| `RecursiveRef` (payload field `depth: Int`), `types` (field name on `Union`/`Intersect` payloads) | `src/typecheck_annot.rs` (`typenode_value_to_type`), `src/builtins_meta.rs` (`is_contractive_value`) | TypeNode constructor for de Bruijn recursive type references (the self-reference inside a `Recursive` type body). `typenode_value_to_type` reads the `"depth"` field (an integer de Bruijn index) from a `RecursiveRef` payload to produce `TypeValue.RecursiveRef { depth: Integer }`. `is_contractive_value` treats a `Value::Variant` with `ctor == "RecursiveRef"` as non-contractive (a bare recursive reference with no guarding constructor). A conforming custom prelude must declare `TypeNode.RecursiveRef` with payload field `depth: Int` for recursive type references to work correctly. |
| `Closed` (tycon `"Closed"`, ctor `"Closed"`) | `src/builtins_async.rs` (`builtin_recv`, `builtin_send`, `builtin_select_once`) | Channel-closed sentinel variant. `builtin-recv` returns `Value::Variant { tycon: "Closed", ctor: "Closed", payload: None }` when the underlying channel is closed (MPSC channel: sender dropped; broadcast channel: all senders dropped; oneshot receiver: sender dropped). `builtin-send` returns the same variant in the `TrySendError::Closed` arm. Any conforming prelude must declare a type `Closed: [type Closed]` and export `Closed.Closed` as the channel-closed constructor to correctly match these return values. The `builtin_async.llt` declaration annotates the channel-closed path with this return type. |
| `"ch"`, `"handler"` (input dict keys), `"ok"`, `"closed"` (output dict keys) | `src/builtins_async.rs` (`builtin_select_once`) | `builtin-select-once` reads input source entries as dicts with two required keys: `"ch"` (the channel value: Channel, BroadcastChannel, or OneshotReceiver) and `"handler"` (a single-argument function called with the received value). On success, `builtin-select-once` returns `Value::Dict { "ok": handler_result }`. When all channels are closed, it returns `Value::Dict { "closed": Value::Dict {} }` (the empty dict as the closed-sentinel value). These four field names are part of the Rust-tinct protocol: any conforming prelude wrapper for `select-once` must structure its source entries with `ch:` and `handler:` fields, and must discriminate the return by checking for the `"ok"` vs `"closed"` key. |

| `structural: "closed-dict"` (typeclass declaration annotation key-value) | `src/typecheck.rs` (`infer_class_decl_from_surface`), constant `STRUCTURAL_CLOSED_DICT` in `src/type_tags.rs` | When a class declaration includes `structural: "closed-dict"` in its annotation dict, the type checker activates `StructuralDischarge::ClosedDict` for that class, enabling structural closed-dict discharge during type checking. Any conforming prelude that declares a typeclass with closed-dict structural discharge behavior must include `structural: "closed-dict"` in the class annotation. The value `"closed-dict"` is the Rust-side protocol string; the key `"structural"` is the field name in the class annotation dict. |
| `"String"`, `"Int"`, `"Float"`, `"Dict"`, `"Bytes"` (recognized type annotation names) | `src/builtins_meta.rs` (`builtin_check_type`) | `builtin-check-type` performs runtime type validation by matching the first argument (a string type annotation name) against the second argument's runtime type. The five recognized names map exactly to `Value` variants: `"String"` → `Value::String { .. }`, `"Int"` → `Value::Int(_)`, `"Float"` → `Value::Float(_)`, `"Dict"` → `Value::Dict(_)`, `"Bytes"` → `Value::Bytes { .. }`. Any other annotation name (type variables, parameterized types, user-defined type names) passes conservatively — the runtime cannot distinguish them without full evaluation. These five names are the tinct-side annotation names that a conforming prelude must use when calling `builtin-check-type` for runtime primitive type assertions. They correspond to the `Value::type_name()` returns for the matching variants (reported as `"String"`, `"Int"`, `"Float"`, `"Dict"`, `"Bytes"`). |

| `builtin-dict-merge` | `src/lower.rs` (`DICT_MERGE_NAME`), `src/builtins_core.rs` | Core builtin used for spread-dict desugaring (`[...a ...b]`). The lowering pass resolves `builtin-dict-merge` from the root group (always present, not from user scope) and emits a left-associative chain of `[builtin-dict-merge seg rest]` calls — right-biased dict merge where entries from `rest` overwrite entries from `seg` for any shared keys. The canonical name is stored in `src/lower.rs` as `pub(crate) const DICT_MERGE_NAME: &str = "builtin-dict-merge"`. Because it is resolved from the root group rather than from user scope, spread-dict desugaring works with any prelude (including an empty prelude) — custom preludes are not required to define `merge` or any other merge function for spread-dict to work. |

| `builtin-typecheck-doc` third argument (env Dict) | `src/builtins_meta.rs` (`builtin_typecheck_doc`) | Optional third argument: an env Dict whose `HashableValue::Str`-keyed entries (in insertion order) are collected and used to build a `GroupSpine::from_flat` that becomes `state.type_stage_eval_group` for the type-stage evaluation. Rust filters the dict for string-keyed thunks in insertion order — non-string keys are ignored. The insertion order of string-keyed entries must match the name-set insertion order used by `builtin-resolve` (called with `root_group_len=0`) for the same document, so that LGM slot `j` in the `GroupSpine` corresponds to env-dict string-keyed entry `j`. A conforming custom loader must pass the accumulated env Dict (built from all previously resolved document entries) as the third argument when calling `builtin-typecheck-doc`. Omitting the third argument causes the type-stage to evaluate without accumulated bindings from prior documents (correct only for the first document in a pipeline). |
| `Document.resolver_frames` (field populated by `builtin-resolve`) | `src/builtins_meta.rs` (`builtin_lower`) | `Value::Document` carries a `resolver_frames` field (populated by `builtin-resolve` as the document's unified scope frames from `resolve_surface_document_with_seed_frames` — env_names frame first, then Dict letrec and BlockBody sequential injection frames in nesting order). `builtin-lower` reads this field to construct cross-dict `MethodDispatcher` closures with correct de Bruijn coordinates. **Invariant:** a `Document` passed to `builtin-lower` must have been processed by `builtin-resolve` first. If `resolver_frames` is empty, `builtin-lower` cannot construct cross-dict method dispatchers correctly and returns an error (`"builtin-lower: document has no resolver frames — call builtin-resolve first"`). The pipeline protocol is strictly: `builtin-resolve` → `builtin-typecheck-doc` (optional) → `builtin-lower` → eval. Calling `builtin-lower` on a freshly parsed `Document` (bypassing `builtin-resolve`) violates this invariant. |

| DirCap flag constructor names: `"Readable"`, `"Statable"`, `"Listable"`, `"Writable"`, `"Appendable"`, `"Deletable"`, `"Renameable"`, `"Symlinkable"`, `"PosixPermissions"`, `"ExtendedAttributes"` | `src/builtins_io.rs` (`builtin_narrow`) | `builtin-narrow` narrows a `DirCap` to a restricted `DirCap` by extracting the set of permitted operations from a variadic list of capability-flag variant values. Rust strips the tycon prefix from the variant's `ctor` field (e.g. `"DirCapFlag.Readable"` → `"Readable"`) and matches the bare constructor name against this fixed set. Each match arm sets the corresponding field of the `DirPerms` struct. An unrecognized bare constructor name produces a runtime error. Any conforming prelude that declares a capability-flag type (e.g. `DirCapFlag: [type Readable Statable ...]`) must use these exact unqualified constructor names — the tycon prefix may be anything, only the constructor suffix is matched. The ten names and their `DirPerms` semantics are part of the Rust-tinct protocol for filesystem capability narrowing. |

| `@Child` annotation (annotation name `"Child"`, role key `"role"`, role values `"One"`/`"MapValues"`, output key `"field-annotations"`) | `src/lower.rs` (`child_role_from_annotation`, `build_field_annotations_core_entry`) | When a constructor parameter field has `@Child` (Simple) or `@Child@[role: "MapValues"]` (Annotated) annotation, the lowerer recognizes `"Child"` as the annotation name, reads the `"role"` key from its payload (if present), and produces a `"field-annotations"` entry in the constructor's CoreExpr dict. The role values `"One"` (single TypeNode child) and `"MapValues"` (dict whose values are TypeNode children) drive how `children`/`map-children` traversal operates in prelude.llt. A custom prelude that declares TypeNode constructor types must use `@Child` and `@Child@[role: "MapValues"]` annotations on fields that hold TypeNode children; `"field-annotations"` and `"role"` are the dict key names that prelude code reads. |

| `CoreExpr::TypeDecl` / `EvalContext.type_identity_registry` / `Arc::ptr_eq` type dispatch | `src/lower.rs` (`lower_type_alias_to_constructor_dict`), `src/eval_core.rs` (TypeDecl evaluator), `src/eval.rs` (`match_pattern` VarRef arm) | **Type identity protocol for user-defined nominal types.** Every `[type Name ...]` declaration is lowered to a `CoreExpr::TypeDecl { type_name, type_decl_id, inner }` node wrapping the constructor dict. Each TypeDecl gets a globally unique `type_decl_id` (u64) at lower time via atomic counter. At evaluation time, the TypeDecl evaluator: (1) creates a fresh `Arc<Value>` identity for the type, (2) registers it in `EvalContext.type_identity_registry` under the `type_decl_id` (NOT the type name), (3) stamps the resulting constructor dict's `type_val` field with this identity Arc. Every `Value::Variant` produced by that type's constructors (via `CoreExpr::UnitVariant { type_decl_id, .. }` and `CoreExpr::Variant { type_decl_id, .. }`) looks up the same identity from the registry by `type_decl_id` and carries it in `type_val`. `match_pattern` uses `Arc::ptr_eq(variant.type_val, dict.type_val)` to test type membership — no string comparison is needed at match time. **Invariant for custom loaders:** any loader that constructs type-constructor dicts without going through `lower_type_alias_to_constructor_dict` and `CoreExpr::TypeDecl` evaluation will produce dicts with `unknown_type_val()` as `type_val`. Such dicts will silently fail all MethodDispatcher user-type dispatch arms (the `Arc::ptr_eq` guard rejects `unknown_type_val()` on both sides). A conforming custom loader MUST lower every named `[type Name ...]` declaration through `lower_type_alias_to_constructor_dict` so the registry is populated before any constructor variants are forced. **B-714:** same-name types in nested scopes produce independent identities via the unique `type_decl_id` — no registry overwrite can occur. |

Note: `class == "Indexable"` dispatch in `src/type_unify.rs` and `"get"`/`"get?"`/`"get-in"` name dispatch in `src/typecheck_cek.rs` were removed in S-992. Neither violation exists in the current codebase.

---

## 8. Adding a New Primitive Type

Checklist:
1. `src/value.rs` — add `Value` variant with Rust representation
2. `stdlib/builtin_core.llt` — add `[type repr: "Value::X"]` declaration in the runtime section; also update `is_valid_repr_string()` in `src/eval_core.rs` (add a new `matches!` arm)
3. `src/typecheck_annot.rs` — add arm to `typenode_value_to_type`
4. `src/imports.rs` — if opaque: the auto-derivation pass in `build_builtin_core_envs_inner` will pick it up automatically from the type-stage scope
5. `src/eval.rs` — add `value_matches_type` dispatch arm if runtime type-checking is needed

Steps 1–3 are mandatory for every new primitive. Steps 4–5 apply only to opaque Rust types.
