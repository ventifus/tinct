# Type System Extensions

For the user-facing annotation syntax, see [Type Annotations](05-type-annotations.md). For the formal inference algorithm, see [Type Inference](06-type-inference.md).

**Terminology note:** The current implementation uses `RowTail` (not `RowRest`). See Part 8 for the migration reference.

## TypeAssert Runtime Validation

The type checker and evaluator must agree on TypeAssert semantics. Currently they diverge: the static check is structural (`is_subtype(actual, expected)` in `resolve_type_assert`), while the runtime check is nominal (string comparison of `value.type_name()`). Record-type assertions like `[@[name: String age: Int] $expr]` pass type checking but are no-ops at runtime — the evaluator only sees "Dict" and cannot validate the record structure.

**Design: full structural convergence.** Both static and runtime TypeAssert checks are structural. The evaluator validates values against the full resolved `Type`, not just a type name string. Record fields are checked lazily via proxy contracts (Findler & Felleisen 2002), preserving tinct's lazy evaluation guarantees.

**Elaboration.** The type checker resolves TypeAssert annotations and embeds the resolved type directly in the AST (Dunfield & Krishnaswami 2021, §elaboration). This follows the standard bidirectional typing approach: the checking judgment produces an elaborated term where type information is explicit.

```
Expr::TypeAssert { expr, annotation }
→ Expr::TypeAssert { expr, annotation, resolved_type: RefCell<Option<Type>> }
```

The parser initializes `resolved_type: None`. The type checker fills it in during `resolve_type_assert()` via `resolve_annotation()`, applying the current substitution to produce a fully-substituted concrete type. Type aliases are resolved at this stage — the evaluator never resolves aliases itself. If type checking is skipped (`--no-typecheck` mode), `resolved_type` remains `None` and the evaluator degrades gracefully (see below).

Because the resolved type is part of the AST, it is captured naturally by `Unevaluated` thunks (which store `expr + env`). No changes to thunk state, `eval()` signatures, or environment structure are required.

**Structural validation judgment.** A judgment `v ∈ τ` ("value v inhabits type τ") defines structural validation at runtime. For primitive types, validation is immediate. For records, validation uses proxy contracts — field types are checked lazily when accessed, not eagerly at the assertion site.

*Immediate rules* (checked at assertion time):

```
────────────────────────────────── [VM-ANY]
v ∈ Unknown

v = Int(n),  n = m
────────────────────────────────── [VM-INT-LIT]
v ∈ IntLiteral(m)

v = Int(_)
────────────────────────────────── [VM-INT]
v ∈ Int

v = Int(_) ∨ v = Float(_)
────────────────────────────────── [VM-NUMBER]
v ∈ Number

v = Float(_)
────────────────────────────────── [VM-FLOAT]
v ∈ Float

v = String(s),  s = t
────────────────────────────────── [VM-STR-LIT]
v ∈ StringLiteral(t)

v = String(_)
────────────────────────────────── [VM-STR]
v ∈ Str

v = Bool(_)
────────────────────────────────── [VM-BOOL]
v ∈ Bool

v = Function{..} ∨ v = Builtin{..}
────────────────────────────────── [VM-FN]
v ∈ Fn(τ₁...τₙ → τᵣ)

v = Seq{..}
────────────────────────────────── [VM-SEQ]
v ∈ Seq(τ)

────────────────────────────────── [VM-VAR]
v ∈ α
```

*Proxy contract rule* (shape checked at assertion time, field types checked lazily):

```
v = Dict(entries),
∀(fᵢ: τᵢ) ∈ fields.  fᵢ ∈ string_keys(entries),
ρ = Closed ⟹ string_keys(entries) = dom(fields),
entries' = { fᵢ ↦ guard(entries[fᵢ], τᵢ, [fᵢ], span) | (fᵢ: τᵢ) ∈ fields }
          ∪ { k ↦ entries[k] | k ∉ dom(fields) }
────────────────────────────────────────────────────── [VM-RECORD-PROXY]
[@τ v] ⟶ Dict(entries')
```

Where `guard(thunk, τ, field_path, span)` creates a guarded thunk — a new `ThunkState::Guarded { inner, expected, field_path, guard_span }` variant in the thunk lifecycle. When materialized, a guarded thunk materializes the inner thunk, validates the result against τ via `v ∈ τ`, and either returns the value (on success) or raises a type assertion error with the field path (on failure). If τ is itself a `Record` type, validation applies [VM-RECORD-PROXY] recursively — the guarded thunk's materialized dict has its own fields wrapped in guards, composing field paths (e.g., `["user", "address", "zip"]`). Guards compose sequentially when nested TypeAsserts wrap the same value (Findler & Felleisen's "guardian stack" semantics).

**Guard memoization.** Guarded thunks follow standard thunk memoization: the type check executes once on first materialization, then the thunk transitions to `Materialized(validated_value)` (or `Failed(error)`). Subsequent accesses return the cached result without re-validation. This is the defunctionalized equivalent of Findler & Felleisen's `mon(τ, e)` contract monitor form.

If materialization of the inner thunk raises an error (e.g., division by zero), that error propagates immediately — it is not a type mismatch and does not trigger `default:`.

**Proxy contracts preserve laziness.** [VM-RECORD-PROXY] performs two phases: (1) *immediate shape validation* — required keys exist, cardinality for closed records — which is eager and runs at the assertion site, and (2) *lazy field type validation* — guard thunks that check field types on access. The key insight from Findler & Felleisen (2002): compound type contracts should defer field checking to the point of observation. A field that is never accessed is never validated — and never forced. This preserves the fundamental lazy evaluation guarantee: unreferenced values are never computed.

```tinct
data: [@[name: String age: Int] [from-json input]]
# Shape check passes immediately (dict has "name" and "age" keys)
# data.name — materializes, guard checks String, returns value
# data.age — never accessed, never forced, never validated
```

**Validation depth by type constructor:**

| Type constructor | Validation | When | Rationale |
|-----------------|-----------|------|-----------|
| `Int`, `Float`, `Number`, `Str`, `Bool` | Exact | Immediate | Primitive — fully checkable |
| `IntLiteral(n)`, `StringLiteral(s)` | Exact | Immediate | Singleton value comparison |
| `Unknown` | Always passes | Immediate | Gradual typing escape hatch |
| `Record(fields, Closed)` | Shape + cardinality | Immediate | Keys present, no extras |
| `Record(fields, Open)` | Shape only | Immediate | Required keys present |
| `Record` field types | Per-field via guard | On access | Proxy contract — lazy |
| `Fn(params → ret)` | Tag only — "is callable" | Immediate | Params/return opaque (Findler-style monitor would be needed for deep checking) |
| `Seq(τ)` | Tag only — "is sequence" | Immediate | Element type opaque; forcing all would diverge |
| `TypeVar(α)` | Always passes | Immediate | Residual from polymorphic instantiation; treated as `Unknown` |

Note on type-level variables: `TypeVar(α)` and `RowVar(r)` are both "variables" but serve different purposes. A `TypeVar` in a field type position indicates unconstrained polymorphism — treated as `Any` at runtime. A `RowVar` in the row rest position indicates structural openness — treated as `Open` at runtime (allow extra fields). `TypeVar` values in `resolved_type` arise only from polymorphic type schemes where a variable was not constrained during inference. Unresolved type aliases produce a `TypeError` during elaboration — they never reach the evaluator as `TypeVar`.

**Function and sequence types are opaque at runtime.** `[@[Fn@Int [String]] f]` verifies that `f` is callable but cannot verify parameter or return types without executing the function. `[@[Seq Int] s]` verifies that `s` is a sequence but cannot verify element types without consuming it (which may diverge for infinite sequences). Both degenerate to tag checks. Full higher-order contract monitoring (Findler & Felleisen 2002) — wrapping functions to check arguments on each call and return values on each return — is outside this design; tinct's proxy contracts apply at record field boundaries, not at function call boundaries.

**Proxy values and TypeAssert.** TypeAssert Record assertions require Dict values. Proxy values produce "expected Record, got Proxy" even though Proxy supports dot access operations. This is by design — TypeAssert validates structural type identity, not access protocol. A Proxy is a handler function wrapped in a value constructor; it does not have a static field set and cannot satisfy shape validation ([VM-RECORD-PROXY] requires enumerating `string_keys(entries)`). To validate a Proxy's output, assert the result of individual field accesses rather than the Proxy itself.

**Closed record cardinality.** `[@[name: String age: Int] expr]` (no `...` rest) is a closed record check: the dict must have exactly the string-keyed fields `name` and `age`, no more, no less. Positional entries (`Key::Int`) are invisible to the Record type (see §Type-theoretic implication) and are excluded from the cardinality check. `[@[name: String ...] expr]` is an open record check: requires `name: String` but allows additional fields. `RowVar(r)` is resolved by the type checker before elaboration; if a row variable remains unresolved at elaboration time, it is treated as `Open`.

**Key type handling.** Record field names are strings, but `Value::Dict` entries use `Key::Int` for positional entries and `Key::String` for named entries. Field lookup during [VM-RECORD-PROXY] shape checking tries `Key::String(fᵢ)` first, then `Key::Int(fᵢ.parse())` as fallback, matching the type checker's Pass 0 key resolution which converts integer literals to strings via `to_string()`.

**Type alias resolution.** TypeAssert annotations may reference type aliases:

```tinct
Person: [type [name: String  age: Int]]
person: [@Person data]
```

The type checker resolves `Person` → `Record([name: Str, age: Int], Closed)` during elaboration and stores the resolved type in `Expr::TypeAssert.resolved_type`. The evaluator reads it directly — no alias registry at runtime.

**Interaction with `default:`.** `default:` is triggered only by type assertion failures, not by computation errors:

- *Shape mismatch* (missing key, cardinality violation): immediate type assertion failure → use `default:` if present, else raise error.
- *Guard failure* (field value has wrong type, detected on access): type assertion error at field access site → use `default:` if present in the original annotation, else raise error.
- *Materialization error* (division by zero, cycle, depth limit during field access): propagates as an exception, bypasses `default:`. Computation failures are distinct from type mismatches (Findler & Felleisen 2002, §blame).

**Default type validation.**

When a TypeAssert includes a `default:` clause, the type checker validates that the default value's type satisfies the asserted type, regardless of whether the main expression's type check succeeds. For example, `[@[type: Number  default: "hello"] 42]` fails at compile time because the default `"hello"` (Str) doesn't satisfy `Number` — even though `42` passes and the default would never be used.

This ensures defaults are always type-safe. A default that doesn't match the asserted type is a latent error: if the main expression changes, the unsound default becomes reachable. The validation uses `is_subtype(default_ty, expected)` with the same structural rules as the main check, so literal types promote correctly (`default: 0` satisfies `type: Number`). This mirrors TypeScript's default parameter validation and reflects the general typed-language principle that fallback values must be type-safe regardless of whether they are reached.

**Interaction with bidirectional checking.** The static type checker uses `check_expr(inner, resolved_type)` for TypeAssert, applying [SUB]: synthesize the inner expression's type, then check `is_subtype(actual, expected)`. The runtime `v ∈ τ` judgment is the dynamic counterpart — it validates the same structural relationships against concrete values.

**Consistency invariant** (for deeply checkable types):

```
If Γ ⊢ e ⇒ σ  and  σ <: τ  and  eval(e) = v  and  τ is deeply checkable,
then v ∈ τ.
```

A type τ is *deeply checkable* when all constituents are fully observable at runtime: primitives, singleton literals, records (recursively), and `Unknown`. The invariant holds because `is_subtype` is more restrictive than `v ∈ τ` for these types.

For *opaque* type constructors (`Fn`, `Seq`), the invariant degenerates to tag-level soundness: [VM-FN] and [VM-SEQ] perform only tag checks, so they accept values that `is_subtype` would reject (e.g., `Fn(Int→Int) ∈ Fn(String→String)` succeeds at runtime). The forward direction still holds: if `is_subtype(σ, τ)` passes statically, the tag check will certainly pass at runtime. But the converse does not — runtime tag success does not imply static subtyping.

**Error messages.** Runtime validation errors report the structural path to the mismatch:

```
type assertion failed: expected [name: String  age: Int],
  field "age": expected Int, got String
```

For guard failures (detected on field access), the error includes the field path. For nested records, paths compose: `field "user"."address"."zip": expected Int, got String`.

**`--no-typecheck` mode.** When type checking is skipped, `resolved_type` is `None`. The evaluator falls back to the current nominal behavior:

- Primitive type assertions (`Int`, `Float`, `String`, `Bool`, `Number`) still work — the annotation name is parsed directly and compared against `value.type_name()`. These are unaffected.
- Structural type assertions (`Record`, `Fn` with param types, `Seq` with element type) degrade to tag-only checks (`type_name() == "Dict"`, etc.) — no structural validation, no guard wrapping.

**Implementation changes summary:**

| Component | Current | After |
|-----------|---------|-------|
| `Expr::TypeAssert` | `{ expr, annotation }` | `{ expr, annotation, resolved_type: RefCell<Option<Type>> }` |
| Parser | — | Sets `resolved_type: None` |
| `resolve_type_assert()` | Returns resolved `Type` | Also sets `resolved_type` on the AST node |
| `eval()` TypeAssert branch | Extracts type name string, compares via `type_name()` | Reads `resolved_type`; primitives → `value_matches_type`; records → shape check + guard wrapping |
| `eval()` signature | Unchanged | Unchanged (no new parameters) |
| New: `value_matches_type()` | — | `fn(&Value, &Type) -> bool` — immediate primitive rules only; no span or error return |
| New: `Thunk::new_guarded()` | — | `fn(Rc<Thunk>, Type, Vec<String>, Span) -> Thunk` — creates a `Thunk` in `Guarded` state; caller wraps in `Rc` |
| New: `ThunkState::Guarded` | — | `{ inner: Rc<Thunk>, expected: Type, field_path: Vec<String>, guard_span: Span }` |
| `type_name()` | Used for TypeAssert validation | Retained for error messages and `--no-typecheck` fallback |
| TypeAssert error messages | "expected Int, got String" | Structural path: "field \"age\": expected Int, got String" |
| `--no-typecheck` mode | Nominal check for all types | Nominal check for primitives, tag-only for structural types |

**References.** Findler, R. & Felleisen, M. (2002). "Contracts for Higher-Order Functions." Strickland, T.S., Tobin-Hochstadt, S., Findler, R. & Felleisen, M. (2012). "Chaperones and Impersonators: Run-time Support for Reasonable Interposition." Wadler, P. & Findler, R. (2009). "Well-Typed Programs Can't Be Blamed." Siek, J. & Taha, W. (2006). "Gradual Typing for Functional Languages." Dunfield, J. & Krishnaswami, N. (2021). "Bidirectional Typing."

## Dual-Dispatch Builtins

**Dual-dispatch operations** (`$map`, `$filter`, `$take`, `$drop`, `$reduce`, `$join`) accept both Dict and Seq inputs and produce different output types depending on the input. The type checker assigns these builtins type `Unknown` because:

1. Tinct has no union types — the precise input type `Dict | Seq` cannot be expressed
2. Separate functions (`$map-dict`, `$map-seq`) would be verbose and break the polymorphic API
3. Overloaded function types would require type system extensions (type classes or similar)
4. `Unknown` is already used for other inherently dynamic operations (e.g., `$from-json`)

Type assertions (`[@Type $expr]`) provide a runtime narrowing mechanism when concrete types are needed. With union types, `$try` can return a precise `[ok: τ] | [err: Str]` result, enabling static reasoning over the dual-dispatch return type.

### Detailed Dispatch Table

Several builtins dispatch on their input type (Dict vs Seq), producing different output types depending on the input:

| Builtin | Dict input | Seq input |
|---------|------------|-----------|
| `$map` | Dict (same keys, lazy PendingCall values) | Seq (lazy transform) |
| `$filter` | Seq (must evaluate predicates) | Seq (lazy filter) |
| `$take` | Dict (first n entries by insertion order) | Seq (first n elements) |
| `$drop` | Dict (skip first n entries by insertion order) | Seq (skip first n elements) |
| `$reduce` | Single value (accumulated over entries) | Single value (accumulated over elements) |
| `$join` | String (concatenates values) | String (concatenates elements) |

**Type system strategy: `Unknown` for all dual-dispatch builtins.** The type checker assigns type `Unknown` to these operations because:

1. **No union types.** The precise input type would be `Dict | Seq`, which cannot be expressed without union types. There is no way to accurately represent "accepts either Dict or Seq."

2. **Separate functions would be verbose.** Naming conventions like `$map-dict` and `$map-seq` would work but break the clean, polymorphic API.

3. **Overloaded function types require type system extensions.** True ad-hoc polymorphism (overloading) requires type classes or similar mechanisms — see §Expressiveness in §Type System Extension Roadmap.

4. **`Unknown` is handled uniformly.** Builtins that cannot be precisely typed (e.g., `$from-json`) use `Unknown`, and type assertions (`[@Type $expr]`) provide a runtime narrowing mechanism.

If the type system gains union types or type classes, dual-dispatch builtins can be typed more precisely — see §Expressiveness.

**`Failed` thunk state:**

To cache evaluation failures instead of restoring `Unevaluated` and re-evaluating on every access attempt:

```
Failed(Box<EvalError>)
```

When a thunk fails to materialize (any state → error), it transitions to `Failed` and stores the error. Future materialization attempts return a clone of the cached error with the `materialization_span` updated to reflect the current access location, preserving the original stack frames. This matches Nix's `nFailed` pattern and prevents quadratic behavior when multiple accesses trigger the same failing computation.

**`PendingBuiltin` preserves laziness:** When the evaluator encounters `[call $builtin ...]`, it does not immediately execute the builtin. Instead, it wraps the builtin name and unevaluated argument thunks in a `PendingBuiltin` state. The builtin executes only when the result is materialized (accessed). This deferred execution is critical for preserving lazy semantics — builtins like `$if` can selectively materialize arguments, and operations like `$map` can return lazy structures without forcing computation.

This completes the laziness picture:

| Thunk state | Represents | Created by |
|-------------|-----------|-----------|
| `Unevaluated` | AST expression + environment | Parser/eval (dict values, fn bodies) |
| `PendingBuiltin` | Deferred builtin call | `[call $builtin ...]` |
| `PendingCall` | Deferred function application | `$map`, `$update`, lazy combinators |
| `InProgress` | Cycle detection sentinel | Materialization |
| `Materialized` | Computed value | After first force |
| `Failed` | Cached evaluation error | Any failed materialization |

**Impact on existing operations:**

With `PendingCall` and `Seq`, several operations become lazier:

| Operation | Before | After |
|-----------|--------|-------|
| `$map f dict` | Eager, O(n^2) | Lazy dict with PendingCall values, O(n) construct / O(1) per access |
| `$filter pred dict` | Eager, O(n^2) | Returns Seq, O(1) construct / O(n) to fully consume |
| `$range start end` | Eager dict, O(n^2) | Seq, O(1) to construct |
| `$range start` | Not possible | Infinite Seq, O(1) |
| `$merge a b` | Eager clone | Lazy overlay (b's keys shadow a's, no deep copy) |
| `$if cond t f` | Materializes chosen branch | Returns chosen branch as thunk |
| `$update dict k f` | Eager | PendingCall on the updated value |

**BuiltinFn signature change:**

To support builtins that return lazy results, `BuiltinFn` changes from returning `Value` to returning `Rc<Thunk>`:

```
// Before
type BuiltinFn = fn(args, named, depth, call_span) -> Result<Value, Box<EvalError>>;

// After
type BuiltinFn = fn(args, named, depth, call_span) -> Result<Rc<Thunk>, Box<EvalError>>;
```

Builtins that currently return materialized values wrap them in `Thunk::new_materialized()`. Builtins like `$map` and `$if` can now return thunks directly. This removes the forced materialization boundary that currently prevents builtins from participating in lazy evaluation.

**Rationale:** The current signature forces all builtins to return fully materialized values, which prevents operations like `$if` from returning the chosen branch as a thunk, and prevents `$map` from returning a dict with lazy PendingCall values. Changing the return type to `Rc<Thunk>` allows builtins to participate in lazy evaluation while maintaining backward compatibility (wrap in `Thunk::new_materialized()` for eager builtins).

**Type inference is unchanged** — return types are determined by unifying the call signature during type checking, not by inspecting returned thunk contents. This change is a runtime optimization only.

**Performance trade-off:** Inherently materializing builtins (~60% of the 28 current builtins: arithmetic, string ops, comparisons) pay two extra heap allocations per call (Thunk + Rc wrapper) to wrap their `Value` result. For lazy-capable builtins (`$if`, `$merge`, `$map`, `$update`), this eliminates the forced materialization boundary. Net benefit when lazy operations dominate. If profiling shows the overhead is significant, a dual-signature approach (`EagerBuiltinFn` vs `LazyBuiltinFn`) could be considered.

## Row-Variable Unification — Kinded Rémy Model (Dict+Tail Representation)

Replace the current closed-strict/open-lenient record unification with kinded row-variable unification following Rémy (1994). Row variables become first-class participants in type inference with a separate **Row kind**, enabling the type checker to infer record extension and restriction through polymorphic function boundaries. The design omits Rémy's presence/absence flags (tinct has no typed field deletion) but preserves the kind separation that makes the soundness proof clean and leaves the door open for full Rémy if typed field deletion is needed later.

**Representation choice:** The Row type uses a **dict+tail** representation (field map plus tail variable) rather than Rémy's cons-list (`Extend(l, τ, ρ)`). Rémy's left-commutativity equations (`l₁:τ₁ ; l₂:τ₂ ; ρ ≡ l₂:τ₂ ; l₁:τ₁ ; ρ`) make rows semantically unordered — the dict+tail representation computes directly in the quotient algebra of rows under these equations, representing each equivalence class as a single canonical form (unordered field map) rather than an arbitrary representative (ordered cons-list). This eliminates the need for a field extraction operation during unification and prevents duplicate labels structurally (the map enforces unique keys). Both representations encode the same abstract algebra; the choice is operational, not theoretical. Bernstein (2024) uses this representation; PureScript and Elm use similar approaches internally.

**Design rationale:** Rémy (1994) Theorem 4.7 proves principal type existence for the kinded row system. The kind separation prevents the class of soundness bugs found in Elm (issue #656, open since 2015) where row variables and type variables are conflated. Wand (1987, Theorem 1, corrected 1988) proves completeness for the presence-only restriction (no absence flags), which is a subsystem of Rémy's full system. PureScript demonstrates that kinded rows work at production scale. Nickel (Rust-based config language) validates kinded row polymorphism in a Rust codebase similar to tinct's.

### Part 1: Row Kind

**Notation:** This section uses ρ for row variables, following Rémy (1994) and Wand (1987). The [Evaluation](08-evaluation.md) §Scope Chain Semantics section uses ρ for environments, following Launchbury (1993). The two uses are confined to separate sections and do not interact — the row-variable ρ participates in type inference, while the environment ρ participates in evaluation.

Rows are a **separate sort** from types. A row maps labels to types with an optional tail variable:

```rust
#[derive(Debug, Clone, Eq)]
pub enum RowTail {
    Empty,              // closed row — no more fields
    RowVar(String, u32), // ρ — row variable (name, Kiselyov generalization level)
}

// PartialEq ignores the level field — two RowVars with the same name are equal regardless of level.
// Level is a bookkeeping field for generalization, not part of structural identity.
impl PartialEq for RowTail {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (RowTail::Empty, RowTail::Empty) => true,
            (RowTail::RowVar(n1, _), RowTail::RowVar(n2, _)) => n1 == n2,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub fields: HashMap<String, Type>,   // known fields {l₁: τ₁, l₂: τ₂, ...}
    pub tail: RowTail,                   // Empty (closed) or RowVar(ρ) (open)
}
```

The `Type` enum changes to reference `Row`:

```rust
pub enum Type {
    // ... existing variants unchanged ...
    Record(Row),   // was Record(IndexMap<String, Type>, RowRest)
    // ...
}
```

**Kind grammar:**

```
κ ::= Type                  # kind of types (Int, String, Record(ρ), ...)
    | Row                   # kind of rows ({x: Int, ...ρ}, {}, ...)
```

Row variables have kind `Row`; type variables have kind `Type`. The substitution enforces this: `type_map: HashMap<String, Type>` and `row_map: HashMap<String, Row>` are separate maps. A type variable can never be bound to a row, and a row variable can never be bound to a type — this invariant is structural (enforced by Rust's type system), not checked at runtime.

**Uniqueness invariant.** The `HashMap<String, Type>` structurally prevents duplicate labels — each label maps to exactly one type. In Rémy's full system, this property is maintained by the presence/absence discipline (each label appears once, flagged as Present or Absent). The dict+tail representation achieves the same invariant through the map data structure. This eliminates the class of bugs where cons-list extraction leaves duplicate labels in row remainders.

**Relationship to evaluation.** The `Row` type exists only in the type system (`types.rs`, `typecheck.rs`). The evaluator continues to use `IndexMap<Key, Rc<Thunk>>` for runtime dicts. There is no `Row` at runtime — the type-level row is erased during type checking. This separation is standard: PureScript and OCaml both use different representations for type-level rows and runtime records.

**Forward compatibility with full Rémy.** If typed field deletion is needed in the future, the field map gains presence flags:

```rust
pub enum FieldPresence { Present, Absent }

pub struct Row {
    pub fields: HashMap<String, (FieldPresence, Type)>,  // l → (pre(τ) | abs)
    pub tail: RowTail,
}
```

The current design is a strict subset — every field entry `l: τ` is implicitly `l: (Present, τ)`. Adding the flag later requires updating field access patterns but not the unification algorithm structure. The extract-and-recurse flow gains a presence-compatibility check (Present must match Present), but the overall partitioning and tail-binding logic is preserved.

### Part 2: Substitution and Occurs Check

The substitution splits into two kinded maps:

```rust
pub struct Substitution {
    pub type_map: HashMap<String, Type>,   // α → τ  (kind: Type)
    pub row_map: HashMap<String, Row>,     // ρ → r  (kind: Row)
}
```

**Application** (`apply`) walks types and rows, replacing bound variables from the appropriate map:

**Note on `Function` param representation:** `Type::Function` stores params as `Vec<(Option<String>, Type)>` — each entry is a `(name, type)` pair where `name` is `Some(param_name)` for user-defined functions and `None` for builtins. All traversals (apply, occurs check, substitution) must destructure each tuple as `(_name, ty)` and recurse only into `ty`; the name component is never a type and is never traversed. Implementors adding new traversal sites must follow this pattern.

```
apply_type(τ, S):
  TypeVar(α)      → if α ∈ S.type_map then apply_type(S.type_map[α], S) else TypeVar(α)
  Record(r)       → Record(apply_row(r, S))
  Function(ps, r) → Function(map((_name, ty) → (_name, apply_type(ty, S)), ps), apply_type(r, S))
  Seq(τ)          → Seq(apply_type(τ, S))
  otherwise       → τ  (Int, Float, Str, Bool, Number, Unknown, literals)

apply_row(Row { fields, tail }, S):
  fields' = { l: apply_type(τ, S) for (l, τ) in fields }
  match tail:
    Empty       → Row { fields: fields', tail: Empty }
    RowVar(ρ)   → if ρ ∈ S.row_map:
                     let bound = apply_row(S.row_map[ρ], S)
                     Row { fields: merge(fields', bound.fields), tail: bound.tail }
                   else:
                     Row { fields: fields', tail: RowVar(ρ) }
```

The `merge` is **left-biased**: explicit fields (from `fields'`) take precedence over inherited fields (from `bound.fields`). Concretely, a field from the bound row is included only if `fields'` does not already contain that key. Duplicates can legitimately arise when a row variable is bound (by a prior unification step or direct construction) to a row that re-introduces a field already present in the explicit fields; the left-bias ensures the explicit field always wins.

**Occurs check** is per-kind:

```
type_var_occurs(α, τ):
  TypeVar(β)        → α == β
  Record(r)         → type_var_occurs_in_row(α, r)
  Function(ps, r)   → any(type_var_occurs(α, ty) for (_name, ty) in ps) ∨ type_var_occurs(α, r)
  Seq(τ)            → type_var_occurs(α, τ)
  otherwise         → false

row_var_occurs(ρ, Row { fields, tail }):
  # Note: The implementation threads a `visited: &mut HashSet<String>` argument to prevent
  # infinite loops on cyclic substitutions. The pseudocode omits this bookkeeping.
  (any(row_var_occurs_in_type(ρ, τ) for τ in fields.values()))
  ∨ match tail:
      RowVar(σ) → ρ == σ
      Empty     → false

row_var_occurs_in_type(ρ, τ):
  # S = ambient substitution (implicit parameter — passed through from the call site)
  Record(r)       → row_var_occurs(ρ, r)
  Function(ps, r) → any(row_var_occurs_in_type(ρ, ty) for (_name, ty) in ps)
                     ∨ row_var_occurs_in_type(ρ, r)
  Seq(τ)          → row_var_occurs_in_type(ρ, τ)
  TypeVar(α)      → if α ∈ S.type_map: row_var_occurs_in_type(ρ, S.type_map[α])
                     else: false
  otherwise       → false

type_var_occurs_in_row(α, Row { fields, tail }):
  any(type_var_occurs(α, τ) for τ in fields.values())
  # tail is a RowVar or Empty — neither contains type variables
```

The row-variable occurs check traverses **both** the tail (preventing direct infinite rows like `ρ = {x: Int, ...ρ}`) **and** field types (preventing infinite types through nesting like `ρ = {x: Record({y: Int, ...ρ})}` — where binding ρ to this row would create an infinite structure). This is necessary because `Record(Row)` embeds a row inside a type, so a row variable can appear transitively inside a field type via nesting.

### Part 3: Row Unification

Row unification is the core of the design. It uses **field partitioning** — given two rows, partition their fields into shared (present in both) and unique (present in only one), then unify shared field types and bind row variable tails to the other side's unique fields. This directly computes in the quotient algebra of Rémy's left-commutativity equations.

**Unification algorithm:**

```
unify_rows(Row { fields: F₁, tail: t₁ }, Row { fields: F₂, tail: t₂ }, S):
  # Step 1: Resolve bound row variables
  (F₁, t₁) = resolve_row(F₁, t₁, S)
  (F₂, t₂) = resolve_row(F₂, t₂, S)

  # Step 2: Partition fields
  shared  = F₁.keys() ∩ F₂.keys()
  unique₁ = { l: F₁[l] for l in F₁.keys() \ shared }
  unique₂ = { l: F₂[l] for l in F₂.keys() \ shared }

  # Step 3: Unify shared field types
  for l in shared:
    S = unify_types(F₁[l], F₂[l], S)

  # Step 3.5: Re-resolve tails after shared-field unification
  # Step 3's recursive unify_types() calls may have bound row variables that appear
  # as t₁ or t₂ (e.g., when unifying nested Record types that share a row variable
  # with the outer row's tail). Re-resolve to surface any new bindings.
  (unique₁, t₁') = resolve_row(unique₁, t₁, S)
  (unique₂, t₂') = resolve_row(unique₂, t₂, S)

  # Step 3.6: Re-partition after re-resolution
  # Re-resolution may surface new fields from row variable bindings that overlap
  # with the other side's unique fields. Unify these new shared fields.
  new_shared = unique₁.keys() ∩ unique₂.keys()
  if new_shared ≠ ∅:
    for l in new_shared:
      S = unify_types(unique₁[l], unique₂[l], S)
    # Remove newly-shared fields from unique sets
    unique₁ = { l: unique₁[l] for l in unique₁.keys() \ new_shared }
    unique₂ = { l: unique₂[l] for l in unique₂.keys() \ new_shared }
    # Recursive call with updated remainders
    return unify_rows(Row { fields: unique₁, tail: t₁' },
                      Row { fields: unique₂, tail: t₂' }, S)

  # Step 4: Unify remainders (unique fields + tails)
  S = unify_remainders(unique₁, t₁', unique₂, t₂', S)
  return S

resolve_row(fields, tail, S):
  match tail:
    RowVar(ρ) if ρ ∈ S.row_map →
      let bound = apply_row(S.row_map[ρ], S)
      return (fields ∪ (bound.fields \ dom(fields)), bound.tail)   # explicit fields win
    _ → return (fields, tail)
```

**Remainder unification** handles the four cases from Wand (1987):

```
unify_remainders(U₁, t₁, U₂, t₂, S):
  # Note: Case 4 must be matched before Cases 2/3 in implementation
  # to prevent pattern shadowing (Case 2 is strictly more general than Case 4).
  match (U₁.is_empty(), t₁, U₂.is_empty(), t₂):

    # Case 1: No unique fields on either side — unify tails directly
    (true, _, true, _) →
      unify_tails(t₁, t₂, S)

    # Case 4: Both have unique fields and different row vars — create fresh row variable for shared tail
    # Occurs check prevents infinite rows: if ρ₁ appears in U₂'s field types
    # (e.g., ρ₁ = {x: Record({y: Int, ...ρ₁}), ...ρ_fresh}), binding ρ₁ would
    # create an infinite structure. On failure: emit a type error "infinite row
    # type: ρ₁ occurs in its own binding", halt unification.
    (false, RowVar(ρ₁), false, RowVar(ρ₂)) when ρ₁ ≠ ρ₂ →
      let ρ_fresh = fresh row variable
      if row_var_occurs(ρ₁, Row(U₂, RowVar(ρ_fresh))): ERROR infinite row
      if row_var_occurs(ρ₂, Row(U₁, RowVar(ρ_fresh))): ERROR infinite row
      S ∪ {ρ₁ → Row { fields: U₂, tail: RowVar(ρ_fresh) }}
        ∪ {ρ₂ → Row { fields: U₁, tail: RowVar(ρ_fresh) }}

    # Case 2: Only left has unique fields — right tail must absorb them
    # The `true` in position 3 already encodes that U₂ is empty; Case 4 is checked first
    # in implementation to prevent shadowing when both sides have unique fields.
    (false, _, true, RowVar(ρ₂)) →
      if row_var_occurs(ρ₂, Row(U₁, t₁)): ERROR infinite row
      S ∪ {ρ₂ → Row { fields: U₁, tail: t₁ }}

    # Case 3: Only right has unique fields — left tail must absorb them
    # The `true` in position 1 already encodes that U₁ is empty; Case 4 is checked first
    # in implementation to prevent shadowing when both sides have unique fields.
    (true, RowVar(ρ₁), false, _) →
      if row_var_occurs(ρ₁, Row(U₂, t₂)): ERROR infinite row
      S ∪ {ρ₁ → Row { fields: U₂, tail: t₂ }}

    # Case 5: Left side has unique fields but right tail is closed — extra fields not allowed
    (false, _, _, Empty) → ERROR: extra fields {U₁.keys()} in closed row
    # Case 6: Right side has unique fields but left tail is closed — extra fields not allowed
    (_, Empty, false, _) → ERROR: extra fields {U₂.keys()} in closed row

    # Case 7: Same row variable but both sides have unique fields — impossible constraint
    # e.g., {x: Int, ...ρ} ~ {y: Str, ...ρ} would require ρ to simultaneously
    # provide both x and y, which is impossible since ρ is a single row variable.
    (false, RowVar(ρ₁), false, RowVar(ρ₂)) when ρ₁ == ρ₂ →
      ERROR: incompatible fields {U₁.keys() ∪ U₂.keys()} with shared row variable ρ₁

unify_tails(t₁, t₂, S):
  match (t₁, t₂):
    (Empty, Empty)           → S
    (RowVar(ρ₁), RowVar(ρ₂)) →
      if ρ₁ == ρ₂: S
      else:
        # Symmetric level lowering (Kiselyov 2013): prevent unsound generalization
        levels[ρ₂] := min(levels[ρ₁], levels[ρ₂])
        S ∪ {ρ₁ → Row { fields: {}, tail: RowVar(ρ₂) }}
    (RowVar(ρ), Empty)       → S ∪ {ρ → Row { fields: {}, tail: Empty }}
    (Empty, RowVar(ρ))       → S ∪ {ρ → Row { fields: {}, tail: Empty }}
```

**Case 4** is the key insight from Wand (1987): when both rows have unique fields and open tails, a fresh row variable `ρ_fresh` is created to represent the (yet unknown) fields shared by both tails. Each original tail is bound to the other side's unique fields plus this shared unknown. This correctly propagates constraints — if either tail is later unified with a concrete row, the constraints flow through `ρ_fresh` to the other side. Case 4 must be matched before Cases 2/3 in implementation because Case 2's pattern `(false, _, _, RowVar(ρ₂))` is strictly more general and would shadow Case 4, incorrectly binding only one tail instead of both.

**`partition_fields_and_bind` — Case 4 implementation.** Case 4 is extracted into a dedicated function (`src/types.rs: partition_fields_and_bind`) to keep `unify_remainders` readable. The function takes the two unique field sets and the two RowVar names (ρ₁, ρ₂), and performs the following steps in sequence:

1. **Allocate `ρ_fresh`**: create a fresh row variable at `state.level`, register its level in `state.levels`.
2. **Build binding rows**: `row2_with_fresh = Row { fields: U₂, tail: RowVar(ρ_fresh) }` and `row1_with_fresh = Row { fields: U₁, tail: RowVar(ρ_fresh) }`. Both share the same `ρ_fresh` tail, establishing the linkage.
3. **Occurs checks**: verify `row_var_occurs(ρ₁, row2_with_fresh)` and `row_var_occurs(ρ₂, row1_with_fresh)` are both false. Either check failing indicates a would-be infinite row type (e.g., `ρ₁ = {x: Record({...ρ₁}), ...ρ_fresh}`) and signals a type error.
4. **Level lowering**: call `lower_row_var_levels(row2_with_fresh, level(ρ₁))` and `lower_row_var_levels(row1_with_fresh, level(ρ₂))`. This ensures inner type/row variables cannot escape their binding scope via the fresh tail (Kiselyov 2013 §level-lowering).
5. **Bind and size-check**: `subst.row_map.insert(ρ₁, row2_with_fresh)` followed by `subst.check_size(span)?`, then `subst.row_map.insert(ρ₂, row1_with_fresh)` followed by `subst.check_size(span)?`. The `check_size` call after each insert enforces the global substitution size limit (prevents runaway unification).

The occurs check at step 3 must happen before the level lowering at step 4: if the occurs check fires, no binding occurs and no levels are mutated. Level lowering is a side effect that should only run for successful bindings.

**Type-level unification for records:**

```
UNIFY-RECORD:
  unify_types(Record(r₁), Record(r₂), S) = unify_rows(r₁, r₂, S)
```

All record unification delegates to row unification. The current nine-case `match` in `unify()` for Record (lines 319-340 of types.rs) is replaced by this single delegation.

**Complexity:** Field partitioning is O(n) where n is the total number of fields across both rows (hash-based set operations on HashMap keys). This improves on the cons-list extract-and-recurse approach which is O(n²) worst case (O(n) scan per field). For tinct's use case (configuration records, typically < 100 fields) both are acceptable, but O(n) is strictly better.

### Part 4: Instantiation and Generalization

Row variables participate in generalization and instantiation via the standard HM mechanism, extended to two kinds.

**Variable collection** (two sets):

```
collect_type_vars(τ) → Set<String>     # type variables in τ
collect_row_vars(τ) → Set<String>      # row variables in τ

collect_row_vars(Record(Row { fields, tail })):
  row_vars_in_fields(fields) ∪ row_vars_in_tail(tail)

row_vars_in_fields(fields) = ⋃{ collect_row_vars(τ) for τ in fields.values() }
row_vars_in_tail(RowVar(r)) = {r}
row_vars_in_tail(Empty)     = {}
```

**Instantiation** freshens both namespaces independently:

```
instantiate(τ, counter):
  type_vars = collect_type_vars(τ)
  row_vars = collect_row_vars(τ)
  renaming = Substitution::new()
  for α in type_vars:
    renaming.type_map[α] = TypeVar(fresh_name(counter))
  for ρ in row_vars:
    renaming.row_map[ρ] = Row { fields: {}, tail: RowVar(fresh_name(counter)) }
  return apply_type(τ, renaming)
```

Row variables and type variables use **separate namespaces** — `_t0` is unambiguously a type variable or a row variable depending on which map it appears in. Both share the `_t{n}` naming counter (via `InferState.name_counter`), but are separated by the kinded `type_map` vs `row_map` in Substitution. This separation is enforced structurally by Rust's type system: `type_map: IndexMap<String, Type>` binds type variable names to Type, while `row_map: IndexMap<String, Row>` binds row variable names to Row. A variable name cannot appear in both maps simultaneously during well-formed unification. (User-supplied annotation names that violate kind separation can break this invariant — see `ann_mapping` cross-kind collision in TODO.md.)

**Generalization** (with levels, per [Type Inference](06-type-inference.md) §Let-Generalization): row variables carry levels identically to type variables. A row variable `ρ` with `levels[ρ] > ℓ` is generalized at a let-binding. The `TypeScheme` representation extends to track both:

```rust
pub struct TypeScheme {
    pub type_vars: Vec<String>,    // universally quantified type variables
    pub row_vars: Vec<String>,     // universally quantified row variables
    pub body: Type,
}
```

**Dependency note:** Row-variable generalization and levels-based let-generalization ([Type Inference](06-type-inference.md) §Let-Generalization) are co-dependent: row variables participate in generalization via the same level-based mechanism as type variables.

### Part 5: Access Chain Constraint Generation

With row variables bindable, access chains can generate constraints instead of falling back to `Unknown` (resolving the limitation documented in [Type Inference](06-type-inference.md) §Access Chain Evaluation Part 5).

```
check_dot_access(Γ, e, field) :
  τ = infer(Γ, e)
  τ' = apply_subst(τ)
  match τ':
    Record(Row { fields, tail }) →
      if field ∈ fields: return fields[field]
      else match tail:
        RowVar(ρ) → let β = fresh_type_var()
                     let ρ_fresh = fresh_row_var()
                     S ∪ {ρ → Row { fields: {field: β}, tail: RowVar(ρ_fresh) }}
                     return β
        Empty     → ERROR: field not found in closed record
    TypeVar(α)  → let β = fresh_type_var()
                   let ρ = fresh_row_var()
                   unify(TypeVar(α), Record(Row { fields: {field: β}, tail: RowVar(ρ) }))
                   return β
    Unknown     → Unknown
    _           → ERROR: not a record
```

The TypeVar case is new and important: `$x.name` where `$x` has unknown type `α` generates the constraint `α = Record({name: β, ...ρ})`, binding `α` to a record type with at least field `name`. Multiple accesses like `$x.name` and `$x.age` accumulate constraints naturally — the first binds `α` to `Record({name: β₁, ...ρ₁})`, the second extracts from `RowVar(ρ₁)` and binds `ρ₁` to `Row({age: β₂, ...ρ₂})`, resulting in `α = Record({name: β₁, age: β₂, ...ρ₂})`.

The RowVar case in Record access binds `ρ` to `Row({field: β}, RowVar(ρ_fresh))`, correctly recording the constraint "ρ must contain field with type β, plus whatever else is in ρ_fresh." This is sound because if ρ is later unified with a row that lacks the field, the binding will conflict.

Part 5 is complete as of row-unification-e.

### Part 6: Subtyping

`is_subtype` handles `Record(Row)` directly using the field map:

```
is_subtype(Record(Row { fields: F₁, tail: t₁ }), Record(Row { fields: F₂, tail: t₂ })):
  # All fields in sup must be present in sub with subtype field types
  for (l, τ_sup) in F₂:
    τ_sub = F₁[l] or return false
    if not is_subtype(τ_sub, τ_sup): return false

  # Closed sup requires sub has no extra fields
  match t₂:
    Empty     → F₁.keys() ⊆ F₂.keys()
    RowVar(_) → true    # open via row var — extra fields allowed
```

This preserves the current behavior ([Type Inference](06-type-inference.md) §Subtyping S-REC) while working with the new Row representation. The `RowVar` in subtyping position acts as `Open` — consistent with the gradual typing design where unknown row extensions are permitted.

### Part 7: Display

Row types display using tinct's existing syntax:

```
Display for Row { fields, tail }:
  field_strs = ["{l}: {τ}" for (l, τ) in fields]
  tail_str = match tail:
    Empty     → None
    RowVar(r) → Some(if r.starts_with("_") then "..." else "...{r}")
  parts = field_strs ++ [tail_str].flatten()
  return parts.join("  ")
```

Generated row variable names (from anonymous `...` syntax) are displayed as bare `...` rather than `..._open0` to avoid confusing users with names they didn't write. Named row variables (user-written `...name`) display as `...name`.

Examples:
- `Record(Row { fields: {name: Str, age: Int}, tail: Empty })` → `[name: Str  age: Int]`
- `Record(Row { fields: {name: Str}, tail: RowVar("r") })` → `[name: Str ...r]`
- `Record(Row { fields: {name: Str, age: Int}, tail: RowVar("_open0") })` → `[name: Str  age: Int ...]`
- `Record(Row { fields: {}, tail: Empty })` → `[]`
- `Record(Row { fields: {}, tail: RowVar("rest") })` → `[...rest]`

### Part 8: Migration Reference (Complete)

The migration replaced `RowRest` with `RowTail`, added `Row` as a struct, and changed `Record(IndexMap, RowRest)` to `Record(Row)`:

| Before | After |
|--------|-------|
| `RowRest::Closed` | `RowTail::Empty` |
| `RowRest::Open` | `RowTail::RowVar(fresh)` (anonymous open became named) |
| `RowRest::RowVar(name)` | `RowTail::RowVar(name)` |
| `Record(fields, rest)` | `Record(Row { fields, tail })` |
| `Substitution { map }` | `Substitution { type_map, row_map }` |
| `collect_type_vars` (single set) | `collect_type_vars` + `collect_row_vars` (two sets) |

**`RowRest::Open` elimination.** Anonymous open records (`[name: Str ...]`) became `Record(Row { fields: {name: Str}, tail: RowVar(fresh) })` — the type checker generates a fresh row variable name when resolving `Expr::Rest(None)`. The parser produces `Expr::Rest(None)` for the source syntax; the type checker owns the fresh-name counter and generates `_open{n}` names during type resolution (distinct from the `_t{n}` prefix used for type variables, though both share the same monotonic counter). This made all openness explicit and eliminated the `Open` variant entirely.

**Annotation isolation constraint.** Each annotation containing an anonymous open record (`[x: Int ...]`) gets a fresh row variable generated inline in `resolve_property_dict_as_record`: the name counter is read as `_open{n}` (via `format!("_open{}", state.name_counter)`), the counter is incremented, and the level is registered in `state.levels`. There is no helper method wrapping this logic; the freshening happens at the `Expr::Rest(None)` match arm. This ensures that two annotations with the same shape in different positions (e.g., two function parameters both typed as `[x: Int ...]`) get distinct row variables (`_open3`, `_open4`), preventing spurious constraint propagation. Without this isolation, unifying one annotation's row variable during constraint solving would affect the other annotation's row, causing type errors for structurally identical but semantically independent open records. The isolation is achieved by freshening during the type checking pass, not during parsing — the parser produces `Expr::Rest(None)`, and freshening happens per annotation site, not per source occurrence.

**Structural similarity.** The dict+tail representation was structurally close to the prior `Record(IndexMap<String, Type>, RowRest)` — the field map was preserved as-is, and `RowRest` became `RowTail` with `Closed` → `Empty` and `Open` eliminated. This minimized the migration surface compared to a cons-list representation. Pattern matches on `Record(fields, rest)` became `Record(Row { fields, tail })` — a mechanical transformation.

**Substitution split.** The unification function routes variable bindings to the correct map based on the variable's kind (inferred from context: `TypeVar(α)` → `type_map`, `RowTail::RowVar(ρ)` → `row_map`). Type variables and row variables occupy separate namespaces enforced by the `Substitution` structure.

**Construction.** Inline struct construction is used in the implementation (e.g., `Row { fields: HashMap::new(), tail: RowTail::Empty }`). Helper functions like `Row::closed()` or `Row::var()` were not added.

### Part 9: Properties

**P1 — Principal types.** Every well-typed expression has a principal type under the kinded row unification algorithm. For the presence-only restriction (no absence flags), this follows from Wand (1987, Theorem 1, corrected 1988). The full system with presence/absence flags is covered by Rémy (1994, Theorem 4.7). The dict+tail representation computes in the quotient algebra of Rémy's rows under left-commutativity; since it is isomorphic to the cons-list representation, the principal type theorem applies unchanged.

**P2 — Kind safety.** Type variables and row variables inhabit separate namespaces enforced by the `Substitution` structure (`type_map` vs `row_map`) and by Rust's type system (`Type` vs `Row` are distinct types). A type variable α can never be bound to a row, and a row variable ρ can never be bound to a type. This prevents the class of bugs exemplified by Elm issue #656.

**P3 — Row commutativity.** `{a: Int, b: Str, ...ρ}` unifies with `{b: Str, a: Int, ...ρ}` — field order in rows is irrelevant. This is enforced structurally by the dict+tail representation: the `HashMap` is an unordered field collection, so commutativity is automatic rather than computed via extraction.

**P4 — Occurs check termination.** The per-kind occurs check prevents infinite types (`α = Record({x: α})`) and infinite rows (`ρ = {x: Int, ...ρ}`). The row-variable occurs check traverses field types to prevent infinite structures through nesting (`ρ = {x: Record({y: Int, ...ρ})}`). Combined with the finite-depth property of tinct's AST, unification terminates.

**P5 — Backward compatibility.** All currently well-typed programs remain well-typed. The migration changes internal representation but not the type language visible to users. Programs that previously inferred `Unknown` for row-polymorphic positions will now infer more precise types — this is strictly more informative, not breaking.

**P6 — Forward compatibility with full Rémy.** Adding presence/absence flags changes field map values from `Type` to `(FieldPresence, Type)`. The partitioning algorithm gains a presence-compatibility check (Present must match Present, Absent must match Absent), and field access must skip Absent fields. The overall structure (partition shared/unique, unify shared, bind tails) is preserved. See Part 1: Row Kind for the extension point.

**P7 — Label uniqueness.** The `HashMap<String, Type>` structurally prevents duplicate labels in any row. This invariant is maintained through all operations: construction (from source), unification (partitioning preserves uniqueness), and substitution application (field merging of disjoint maps). No runtime duplicate-label check is needed.

**P8 — Tail-field disjointness.** The fields of a row and the fields of its resolved tail are disjoint at unification time, not after full substitution resolution. When `unify_remainders` binds a tail `ρ` to `Row { fields: U, tail: t }`, the unique fields `U` were computed as the set difference `F_other \ shared` — fields present in the other row but not in the row containing `ρ`. Since `ρ` is the tail of the row that contributed the `shared` fields, and `U` contains only fields *not* in that row, the two sets are disjoint at binding time. However, later unifications may bind row variables in `t`, surfacing new fields that overlap with the row's explicit fields. The implementation handles this via re-resolution and re-partitioning (Steps 3.5 and 3.6 in `unify_rows`), ensuring that overlapping fields are unified as shared fields before passing the truly disjoint remainders to `unify_remainders`.

### Part 10: Formal References

See [doc/17-references.md §Row polymorphism](17-references.md) for full citations of Rémy (1994), Wand (1987), Gaster & Jones (1996), Harper & Pierce (1991), and Bernstein (2024).

## Type System Extension Roadmap

The type system evolves across three areas. Each is independently useful and produces a complete type system.

**Precision.** Register builtin type signatures, add Seq type inference, add error recovery for LSP.

- `TypeEnv::with_builtins()` constructor pre-registering type signatures for all Rust-native builtins. Dual-dispatch builtins (`$map`, `$filter`, etc.) are typed as `Unknown` (matching §Dual-Dispatch Builtins above). Non-overloaded builtins get precise types (e.g., `$+ : Fn(Number, Number → Number)`, `$length : Fn(Unknown → Int)`).
- Seq type inference for sequence-only builtins (`$seq`, `$range`, `$repeat`, `$cycle`, `$iterate`, `$unfold`, `$take`). Annotate return types in `check_call` so LSP hover shows `Seq(Int)` instead of `Unknown`. Dual-dispatch builtins (`$map`, `$filter` on Dict|Seq) remain typed as `Unknown` — precise typing requires type classes or union types (see §Expressiveness).
- `Type::Error` sentinel — a type that propagates silently through inference without generating additional errors. When a subexpression fails type checking, `Type::Error` prevents cascading errors (currently, a single type error can produce 5–10 follow-on errors from dependent expressions). Semantics: `unify(Error, τ) → S` unchanged (no binding, no error), `is_subtype(Error, _) = false`. `Type::Error` is recorded in the type map so LSP hover can show "error" rather than nothing. This is the standard approach used by GHC, Elm, and Rust.

The Precision area does not change any inference rules or subtyping relationships. It extends the type environment and improves error reporting.

**Completeness.** Extend type inference to cover named arguments, detect polymorphic recursion, and fix the function variance inconsistency.

- Named arg unification — **implemented**. `Type::Function` carries `params: Vec<(Option<String>, Type)>`. Named args are matched **by name lookup** (`params.iter().find_map(|(pname, pty)| if pname.as_ref() == Some(arg_name) { Some(pty) })`), not positionally by index. Checking fires in CALL-MONO, `check_call` CALL-POLY, and `check_call_with_scheme` Function arm. Partial gaps remain (tracked in TODO.md): same-dict letrec forward references fall through to the TypeVar arm and skip named-arg validation; the positional-zip arity model does not account for named-arg slot reservation.
- Polymorphic recursion detection — forbid with a clear error message ("polymorphic recursion requires explicit type annotation"), rather than silently diverging during inference. Detection is immediate (depth 1): if a recursive call site instantiates a type variable that was bound by an outer call to the same function, report the error. No partial polymorphic recursion is allowed. This item assumes let-generalization ([Type Inference](06-type-inference.md) §Let-Generalization) is implemented — without let-polymorphism, every recursive call is monomorphic by definition and the detection is vacuous.
- CALL-MONO/CALL-POLY divergence fix — the current dual-path design (unify for CALL-POLY, is_subtype for CALL-MONO) gives different verdicts for the same literal type pair depending on whether type variables are present (see [Type Inference](06-type-inference.md) §CALL-MONO/CALL-POLY literal type divergence). The structural recursive `check_expr` from the bidirectional typing design ([Type Inference](06-type-inference.md) §Bidirectional Typing) resolves this by applying [SUB] at leaves and unification only at actual type variable positions. Note: this is unrelated to function subtyping variance rules (contravariant parameters, covariant return), which are already correctly implemented in `src/types.rs:177-196`.
- Formalize `Unknown` semantics (documentation only) — document the consistency relation that `Unknown` actually implements, distinguishing it from true subtyping. Define what the Gradual Guarantee means for tinct. Identify blame boundaries (TypeAssert, builtin return types, function annotations). See `doc/whatif/gradual-typing.md` for the full analysis.

Other Completeness items (polymorphic recursion detection, CALL-MONO/CALL-POLY divergence fix, `Unknown` formalization) may proceed in parallel with Precision.

**Relationship to other work.** The §Row-Variable Unification and let-generalization ([Type Inference](06-type-inference.md) §Let-Generalization) are separate infrastructure areas, not part of this roadmap. Completeness's polymorphic recursion detection assumes let-generalization is implemented. Row variable binding is complete as of row-unification-e.

**Expressiveness.** Three independent features, each addressed by a specific condition. These are design extensions analyzed in `doc/whatif/` files.

| Feature | Condition | Analysis |
|---------|-----------|----------|
| Gradual typing formalization | `Unknown`-as-top-and-bottom causes a soundness bug that affects users | `doc/whatif/gradual-typing.md` |
| Type classes | User-defined types need to participate in builtin protocols (Eq, Ord, Num) — see `doc/whatif/typeclasses.md` for the accepted design | `doc/whatif/typeclasses.md` |
| Union types | `Unknown` typing for dual-dispatch builtins causes false positives in practice | `doc/whatif/union-types.md` |

Expressiveness features are independent of each other — any can be adopted without the others. The `doc/whatif/` files analyze what each adoption would require, what it would gain and lose, and recommend an implementation approach.
