# Type System Extensions

For the user-facing annotation syntax, see [Type Annotations](05-type-annotations.md). For the formal inference algorithm, see [Type Inference](06-type-inference.md).

**Current design:** Tinct uses Boolean-Algebraic Subtyping (BAS) for record types and union types. See §Boolean-Algebraic Subtyping below for the live specification. The Rémy-style row polymorphism design is documented in the Appendix at the end of this document.

## Boolean-Algebraic Subtyping (BAS)

Tinct's type system uses Boolean-Algebraic Subtyping (BAS), following Chau & Parreaux (POPL 2026). BAS encodes all of union, intersection, negation, and record extension in one distributive Boolean lattice, with all records closed and record openness expressed via width subtyping.

### The BAS Type Algebra

The type grammar is a Boolean algebra over atomic types:

| Formal | tinct annotation | Notes |
|--------|-----------------|-------|
| `A \| B` | `@[A B]` | union — existing positional entries in `@[...]` |
| `A & B` | `@[[all A B]]` | intersection — `[all ...]` prefix in annotation |
| `~A` | `@[[without A]]` | negation — `[without A]` prefix in annotation |
| `⊤` | `@Top` | true supertype |
| `⊥` | `@Never` | true bottom |
| `{f: τ}` | `@[f: τ]` | single-field record — existing annotation |
| `α` | `@a` | type variable — bare lowercase identifier |
| `μα.A` | `@[AliasName ...]` | recursive type alias — bracket form with alias name |

Multi-field annotations are intersections: `@[x: T  y: U]` = `{x: T} ∧ {y: U}`. Width subtyping is a theorem of conjunction elimination (`A & B <: A`), not a special rule.

### Key Rules

**S-RcdTop** (Parreaux & Chau 2022, §2.2.2): `{x: τ} ∨ {y: π} ≡ ⊤` when `x ≠ y`. Unions of records with disjoint field names collapse to the top type. **Consequence:** structural `{ok: T} | {err: String}` is not a discriminated union — it equals ⊤. Use nominal class-tagged unions for ADTs.

**S-ClsBot** (Parreaux & Chau 2022, §2.2.2): `#C₁ & #C₂ ≤ Never` for unrelated nominal class tags. Nominal unions (`Ok@T | Err@String`) remain discriminated.

**C-Var1/2**: Constraints of the form `τ₁ ≤ τ₂ ∨ α` rewrite losslessly to `τ₁ & ~τ₂ ≤ α` using Boolean algebra. This eliminates backtracking and yields principal types.

**Row elimination**: Row variables (`RowTail::RowVar`) are removed. Record extension is derived from conjunction: `{name: Str} & {age: Int}` subsumes `{name: Str}` by `A & B <: A`.

### Nominal Result Type

Under BAS, the Result type must use **nominal variants** to be discriminated:

```tinct
[Result: [type [Ok a] [Err String]]]
=== out
{"Result":{}}
```

`Ok@T | Err@String` is discriminated by S-ClsBot (`#Ok & #Err ≤ Never`). Pattern matching uses nominal patterns `[Ok v]` / `[Err msg]`. The `try` builtin returns `Ok(value)` on success and `Err(message)` on caught error. Structural `{ok: v}` / `{err: msg}` dicts remain valid as plain dicts but are not a discriminated union under BAS.

### Record/Map Split and `Dict`

`Dict` is the BAS union of `Record` and `Map@[K: V]` — two different type constructors, not field-disjoint records, so S-RcdTop does not collapse this union:

```
Dict = Record ∨ Map@[K: V]
```

`Record` uses BAS row intersection for multi-field records. `Map@[K: V]` is a parameterized type constructor for homogeneous maps. `get` on `Map@[K: V]` returns `V | Null` (key may be absent); `get` on `Record` with a known field returns the field type directly (total access).

**`@Dict` resolution:** `@Dict` resolves as `Record(Row{})` (width-subtyping fallback). The `Dict = Record ∨ Map` union form is the semantic target; BAS constraint resolution handles the union at type-checking time.

Dict equality is **order-insensitive structural equality** for both Record and Map: same key set with equal values at each key. This follows from the extensional (finite-map) semantics of both forms under BAS — see §Structural Equality in `doc/whatif/completed/parameterized-dict.md`.

See `doc/feature/boolean-algebraic-subtyping.md` (canonical post-implementation document) and `doc/whatif/completed/boolean-algebraic-subtyping.md` (archived design) for the complete design, and `doc/whatif/completed/parameterized-dict.md` for the Record/Map split implementation.

## TypeAssert Runtime Validation

Both static and runtime TypeAssert checks are structural. The evaluator validates values against the full resolved `Type`, not a type name string. Record fields are checked lazily via proxy contracts (Findler & Felleisen 2002), preserving tinct's lazy evaluation guarantees.

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

**Proxy contracts preserve laziness.** [VM-RECORD-PROXY] performs two phases: (1) *immediate shape validation* — required keys exist, cardinality for closed records — which is eager and runs at the assertion site, and (2) *lazy field type validation* — guard thunks that check field types on access. The key insight from Findler & Felleisen (2002): compound type contracts should defer field checking to the point of observation. A field that is never accessed is never validated — and never materialized. This preserves the fundamental lazy evaluation guarantee: unreferenced values are never computed.

```tinct
data: [@[name: String age: Int] [from-json input]]
# Shape check passes immediately (dict has "name" and "age" keys)
# data.name — materializes, guard checks String, returns value
# data.age — never accessed, never materialized, never validated
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 2:1:5
  |
  1 | data: [@[name: String age: Int] [from-json input]]
    |     ^
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
| `Seq(τ)` | Tag only — "is sequence" | Immediate | Element type opaque; materializing all would diverge |
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
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 3:1:7
  |
  1 | Person: [type [name: String  age: Int]]
    |       ^
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

## Numeric Representation Constraints (`repr:`)

The `repr:` annotation property enforces numeric bit width and signedness constraints at type-checking time. It accepts eight string literals corresponding to Rust's integer types: `"u8"`, `"i8"`, `"u16"`, `"i16"`, `"u32"`, `"i32"`, `"u64"`, `"i64"`.

```tinct
# Type checker ensures port has a numeric type
port@[type: Int  repr: "u16"]: 8080

# repr: without type: — type checker infers numeric constraint
flags@[repr: "u32"]: 0x1F

# Type error: repr: requires numeric type
name@[type: String  repr: "u8"]: "hello"   # ERROR: repr: requires numeric type
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 4:2:30
  |
  2 | port@[type: Int  repr: "u16"]: 8080
    |                              ^
```

**Semantics:**

- **Type-checking:** The type checker verifies that the annotated expression has type `Int`, `Float`, or `Number`. If the inferred type is non-numeric, a type error is raised.
- **Runtime:** The `repr:` property is ignored during evaluation — it does not coerce values or enforce bounds. Runtime integer overflow follows Rust's wrapping semantics regardless of `repr:` width.
- **Use cases:** Document intent for serialization (e.g., protocol buffer field widths), signal bit-packing layouts, annotate configuration schemas with external constraints.

**Implementation:** `src/typecheck_annot.rs:100-127` extracts the `repr:` value from the annotation dict and validates it against the allowed set. The type checker then verifies the annotated expression unifies with a numeric type.

## Dual-Dispatch Builtins

**Dual-dispatch operations** (`$map`, `$filter`, `$take`, `$drop`, `$reduce`, `$join`) accept both Dict and Seq inputs and produce different output types depending on the input. `$try` returns a precise `Ok@T | Err@String` nominal result type under BAS.

User-defined types participate in `=`, `<`, `str`, and arithmetic by declaring `Equatable`, `Comparable`, `Showable`, and `Add`/`Sub`/`Mul`/`Div` instances. Primitive operator dispatch checks the ClassEnv for a registered instance before falling back to the built-in Rust implementation. See `doc/feature/advanced-typeclasses.md`.

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

**Type system strategy: `Mappable` for ad-hoc polymorphic builtins.** `$map : Mappable f => (a → b) → f a → f b` and `$filter : Mappable f => (a → Bool) → f a → f a` are typed via the `Mappable` typeclass, enabling precise types for both Dict and Seq inputs without separate per-type functions. Builtins that are inherently dynamic (e.g., `$from-json`) use `Unknown`; type assertions (`[@Type $expr]`) provide runtime narrowing.

**`Failed` thunk state:**

To cache evaluation failures instead of restoring `Unevaluated` and re-evaluating on every access attempt:

```
Failed(Box<EvalError>)
```

When a thunk fails to materialize (any state → error), it transitions to `Failed` and stores the error. Future materialization attempts return a clone of the cached error with the `materialization_span` updated to reflect the current access location, preserving the original stack frames. This matches Nix's `nFailed` pattern and prevents quadratic behavior when multiple accesses trigger the same failing computation.

**`PendingBuiltin` preserves laziness:** When the evaluator encounters `[call $builtin ...]`, it does not immediately execute the builtin. Instead, it wraps the builtin name and unevaluated argument thunks in a `PendingBuiltin` state. The builtin executes only when the result is materialized (accessed). This deferred execution is critical for preserving lazy semantics — builtins like `$if` can selectively materialize arguments, and operations like `$map` can return lazy structures without materializing computation.

This completes the laziness picture:

| Thunk state | Represents | Created by |
|-------------|-----------|-----------|
| `Unevaluated` | AST expression + environment | Parser/eval (dict values, fn bodies) |
| `PendingBuiltin` | Deferred builtin call | `[call $builtin ...]` |
| `PendingCall` | Deferred function application | `$map`, `$update`, lazy combinators |
| `InProgress` | Cycle detection sentinel | Materialization |
| `Materialized` | Computed value | After first materialization |
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

Builtins that return materialized values wrap them in `Thunk::new_materialized()`. Builtins like `$map` and `$if` return thunks directly. This removes the eager materialization boundary that separates builtins from lazy evaluation.

**Rationale:** The `Rc<Thunk>` return type allows builtins to participate in lazy evaluation while maintaining backward compatibility (wrap in `Thunk::new_materialized()` for eager builtins). Operations like `$if` return the chosen branch as a thunk; `$map` returns a dict with lazy PendingCall values — neither requires eager materialization.

**Type inference is unchanged** — return types are determined by unifying the call signature during type checking, not by inspecting returned thunk contents. This change is a runtime optimization only.

**Performance trade-off:** Inherently materializing builtins (~60% of the 28 current builtins: arithmetic, string ops, comparisons) pay two extra heap allocations per call (Thunk + Rc wrapper) to wrap their `Value` result. For lazy-capable builtins (`$if`, `$merge`, `$map`, `$update`), this eliminates the eager materialization boundary. Net benefit when lazy operations dominate. If profiling shows the overhead is significant, a dual-signature approach (`EagerBuiltinFn` vs `LazyBuiltinFn`) could be considered.

---

# Appendix: Archived Rémy Row Polymorphism Design

> **Note:** The following sections describe the Rémy row polymorphism design. Under the current BAS implementation, the `RowTail` enum, `row_map` in `Substitution`, and `row_vars` in `TypeScheme` are not present. All records are closed — openness is expressed via width subtyping in `is_subtype()`. See §Boolean-Algebraic Subtyping above for the live specification.

## Row-Variable Unification — Kinded Rémy Model (ARCHIVED)

This design uses kinded row-variable unification following Rémy (1994). Row variables are first-class participants in type inference with a separate **Row kind**, enabling the type checker to infer record extension and restriction through polymorphic function boundaries.

**Representation choice:** The Row type used a **dict+tail** representation (field map plus tail variable) rather than Rémy's cons-list (`Extend(l, τ, ρ)`). Rémy's left-commutativity equations (`l₁:τ₁ ; l₂:τ₂ ; ρ ≡ l₂:τ₂ ; l₁:τ₁ ; ρ`) make rows semantically unordered — the dict+tail representation computes directly in the quotient algebra of rows under these equations, representing each equivalence class as a single canonical form (unordered field map) rather than an arbitrary representative (ordered cons-list).

**Design rationale:** Rémy (1994) Theorem 4.7 proves principal type existence for the kinded row system. Wand (1987, Theorem 1, corrected 1988) proves completeness for the presence-only restriction (no absence flags). However, BAS (Dolan 2017, Parreaux 2020, Chau & Parreaux 2026) provides a more powerful framework where extensible records emerge from the Boolean algebra of types without needing separate row variables.

### Part 1: Row Kind (ARCHIVED)

**Notation:** This section uses ρ for row variables, following Rémy (1994) and Wand (1987). The [Evaluation](08-evaluation.md) §Scope Chain Semantics section uses ρ for environments, following Launchbury (1993). The two uses are confined to separate sections and do not interact — the row-variable ρ participates in type inference, while the environment ρ participates in evaluation.

Rows were a **separate sort** from types. A row mapped labels to types with an optional tail variable. Under BAS, the `Row` struct is simplified to just fields — no tail:

```rust
// CURRENT (BAS): All records are closed — no tail variable
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub fields: HashMap<String, Type>,
}

// ARCHIVED (Rémy): The following types no longer exist in the codebase
// enum RowTail {
//     Empty,
//     RowVar(String, u32),  // ρ — row variable (name, Kiselyov generalization level)
// }
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

### Part 2: Substitution and Occurs Check (ARCHIVED)

> **Note:** Under BAS, all records are closed — `Substitution` operates only on type variables, not row variables. The `row_map` described below is not present in the current implementation.

The Rémy design split substitution into two kinded maps:

```rust
pub struct Substitution {
    pub type_map: HashMap<String, Type>,   // α → τ  (kind: Type)
    // row_map removed — BAS uses closed records only
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

### Part 3: Row Unification (ARCHIVED)

> **Note:** Under BAS, record unification is handled via `is_subtype()` checks using Boolean algebra rules, not field partitioning. The row unification algorithm below is not active in the current implementation.

In the Rémy design, row unification was the core of the type system. It used **field partitioning** — given two rows, partition their fields into shared (present in both) and unique (present in only one), then unify shared field types and bind row variable tails to the other side's unique fields. This directly computed in the quotient algebra of Rémy's left-commutativity equations.

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

### Part 4: Instantiation and Generalization (ARCHIVED)

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

Row variables and type variables use **separate namespaces** — `_t0` is unambiguously a type variable or a row variable depending on which map it appears in. Both share the `_t{n}` naming counter (via `InferState.name_counter`), but are separated by the kinded `type_map` vs `row_map` in Substitution. This separation is enforced structurally by Rust's type system: `type_map: IndexMap<String, Type>` binds type variable names to Type, while `row_map: IndexMap<String, Row>` binds row variable names to Row. A variable name cannot appear in both maps simultaneously during well-formed unification. (User-supplied annotation names that violate kind separation can break this invariant — the `ann_mapping` cross-kind collision is a known limitation.)

**Generalization** (with levels, per [Type Inference](06-type-inference.md) §Let-Generalization): In the Rémy design, row variables carried levels identically to type variables. Under BAS, `TypeScheme` carries only type variables:

```rust
pub struct TypeScheme {
    pub type_vars: Vec<String>,    // universally quantified type variables
    // row_vars removed — BAS uses closed records, no row variable generalization
    pub body: Type,
}
```

Generalization now operates only on type variables. Record width subtyping is handled via BAS intersection/union rules, not row polymorphism.

### Part 5: Access Chain Constraint Generation (ARCHIVED)

> **Note:** Under BAS, access chains do not generate row variable constraints. Width subtyping is handled via BAS rules. The constraint generation described below is not active in the current implementation.

In the Rémy design, row variables enabled constraint generation for access chains instead of falling back to `Unknown`.

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

Under BAS, width subtyping replaces row variable constraints for access chains (see §Boolean-Algebraic Subtyping above).

### Part 6: Subtyping (ARCHIVED)

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

### Part 7: Display (ARCHIVED)

> **Note:** Under BAS, all records are closed. The `tail` field is always `Empty`, and row variable display logic is unused. Record types display as field lists only.

In the BAS era, record types display using field-only syntax (no tail):

```
Display for Record(IndexMap<String, Type>):
  field_strs = ["{l}: {τ}" for (l, τ) in fields]
  return "[" + field_strs.join("  ") + "]"
```

Examples (BAS era):
- `Record({name: Str, age: Int})` → `[name: Str  age: Int]`
- `Record({})` → `[]`

Under BAS, record types display using field-only syntax (no row variable tail).

### Part 8: Migration Reference (ARCHIVED)

The Rémy design uses `RowTail` (not `RowRest`), a `Row` struct, and `Record(Row)` (not `Record(IndexMap, RowRest)`). The representation correspondence is:

| Simpler model | Rémy design |
|--------|-------|
| `RowRest::Closed` | `RowTail::Empty` |
| `RowRest::Open` | `RowTail::RowVar(fresh)` (anonymous open is named) |
| `RowRest::RowVar(name)` | `RowTail::RowVar(name)` |
| `Record(fields, rest)` | `Record(Row { fields, tail })` |
| `Substitution { map }` | `Substitution { type_map, row_map }` |
| `collect_type_vars` (single set) | `collect_type_vars` + `collect_row_vars` (two sets) |

**`RowRest::Open` handling.** Anonymous open records (`[name: Str ...]`) use `Record(Row { fields: {name: Str}, tail: RowVar(fresh) })` — the type checker generates a fresh row variable name when resolving `Expr::Rest(None)`. The parser produces `Expr::Rest(None)` for the source syntax; the type checker owns the fresh-name counter and generates `_open{n}` names during type resolution (distinct from the `_t{n}` prefix used for type variables, though both share the same monotonic counter). This makes all openness explicit.

**Annotation isolation constraint.** Each annotation containing an anonymous open record (`[x: Int ...]`) gets a fresh row variable generated inline in `resolve_property_dict_as_record`: the name counter is read as `_open{n}` (via `format!("_open{}", state.name_counter)`), the counter is incremented, and the level is registered in `state.levels`. There is no helper method wrapping this logic; the freshening happens at the `Expr::Rest(None)` match arm. This ensures that two annotations with the same shape in different positions (e.g., two function parameters both typed as `[x: Int ...]`) get distinct row variables (`_open3`, `_open4`), preventing spurious constraint propagation. Without this isolation, unifying one annotation's row variable during constraint solving would affect the other annotation's row, causing type errors for structurally identical but semantically independent open records. The isolation is achieved by freshening during the type checking pass, not during parsing — the parser produces `Expr::Rest(None)`, and freshening happens per annotation site, not per source occurrence.

**Structural comparison to simpler model.** The dict+tail representation is structurally close to `Record(IndexMap<String, Type>, RowRest)` — the field map is the same, and `RowRest` maps to `RowTail` with the `Open` variant replaced by a named `RowVar`. Pattern matches on `Record(fields, rest)` become `Record(Row { fields, tail })` — a mechanical transformation.

**Substitution split.** The unification function routes variable bindings to the correct map based on the variable's kind (inferred from context: `TypeVar(α)` → `type_map`, `RowTail::RowVar(ρ)` → `row_map`). Type variables and row variables occupy separate namespaces enforced by the `Substitution` structure.

**Construction.** Inline struct construction is used in the implementation (e.g., `Row { fields: HashMap::new(), tail: RowTail::Empty }`). Helper functions like `Row::closed()` or `Row::var()` were not added.

### Part 9: Properties (ARCHIVED)

**P1 — Principal types.** Every well-typed expression has a principal type under the kinded row unification algorithm. For the presence-only restriction (no absence flags), this follows from Wand (1987, Theorem 1, corrected 1988). The full system with presence/absence flags is covered by Rémy (1994, Theorem 4.7). The dict+tail representation computes in the quotient algebra of Rémy's rows under left-commutativity; since it is isomorphic to the cons-list representation, the principal type theorem applies unchanged.

**P2 — Kind safety.** Type variables and row variables inhabit separate namespaces enforced by the `Substitution` structure (`type_map` vs `row_map`) and by Rust's type system (`Type` vs `Row` are distinct types). A type variable α can never be bound to a row, and a row variable ρ can never be bound to a type. This prevents the class of bugs exemplified by Elm issue #656.

**P3 — Row commutativity.** `{a: Int, b: Str, ...ρ}` unifies with `{b: Str, a: Int, ...ρ}` — field order in rows is irrelevant. This is enforced structurally by the dict+tail representation: the `HashMap` is an unordered field collection, so commutativity is automatic rather than computed via extraction.

**P4 — Occurs check termination.** The per-kind occurs check prevents infinite types (`α = Record({x: α})`) and infinite rows (`ρ = {x: Int, ...ρ}`). The row-variable occurs check traverses field types to prevent infinite structures through nesting (`ρ = {x: Record({y: Int, ...ρ})}`). Combined with the finite-depth property of tinct's AST, unification terminates.

**P5 — Type language stability.** The type language visible to users is unchanged by the internal representation. Programs using open-record annotations infer more precise types under row polymorphism than they would under `Unknown` fallback — this is strictly more informative.

**P6 — Forward compatibility with full Rémy.** Adding presence/absence flags changes field map values from `Type` to `(FieldPresence, Type)`. The partitioning algorithm gains a presence-compatibility check (Present must match Present, Absent must match Absent), and field access must skip Absent fields. The overall structure (partition shared/unique, unify shared, bind tails) is preserved. See Part 1: Row Kind for the extension point.

**P7 — Label uniqueness.** The `HashMap<String, Type>` structurally prevents duplicate labels in any row. This invariant is maintained through all operations: construction (from source), unification (partitioning preserves uniqueness), and substitution application (field merging of disjoint maps). No runtime duplicate-label check is needed.

**P8 — Tail-field disjointness.** The fields of a row and the fields of its resolved tail are disjoint at unification time, not after full substitution resolution. When `unify_remainders` binds a tail `ρ` to `Row { fields: U, tail: t }`, the unique fields `U` were computed as the set difference `F_other \ shared` — fields present in the other row but not in the row containing `ρ`. Since `ρ` is the tail of the row that contributed the `shared` fields, and `U` contains only fields *not* in that row, the two sets are disjoint at binding time. However, later unifications may bind row variables in `t`, surfacing new fields that overlap with the row's explicit fields. The implementation handles this via re-resolution and re-partitioning (Steps 3.5 and 3.6 in `unify_rows`), ensuring that overlapping fields are unified as shared fields before passing the truly disjoint remainders to `unify_remainders`.

### Part 10: Formal References (ARCHIVED)

See [doc/17-references.md §Row polymorphism](17-references.md) for full citations of Rémy (1994), Wand (1987), Gaster & Jones (1996), Harper & Pierce (1991), and Bernstein (2024).
