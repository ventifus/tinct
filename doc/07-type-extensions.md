# Type System Extensions

For the user-facing annotation syntax, see [Type Annotations](05-type-annotations.md). For the formal inference algorithm, see [Type Inference](06-type-inference.md).

**Current design:** Tinct uses Boolean-Algebraic Subtyping (BAS) for record types and union types. See §Boolean-Algebraic Subtyping below for the live specification. The Rémy-style row polymorphism design is documented in [doc/whatif/completed/remy-row-polymorphism.md](../whatif/completed/remy-row-polymorphism.md).

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

```text
Dict = Record ∨ Map@[K: V]
```

`Record` uses BAS row intersection for multi-field records. `Map@[K: V]` is a parameterized type constructor for homogeneous maps. `get` on `Map@[K: V]` returns `V | Null` (key may be absent); `get` on `Record` with a known field returns the field type directly (total access).

**`@Dict` resolution:** `@Dict` resolves as `Record(Row{})` (width-subtyping fallback). The `Dict = Record ∨ Map` union form is the semantic target; BAS constraint resolution handles the union at type-checking time.

Dict equality is **order-insensitive structural equality** for both Record and Map: same key set with equal values at each key. This follows from the extensional (finite-map) semantics of both forms under BAS — see §Structural Equality in `doc/whatif/completed/parameterized-dict.md`.

See `doc/feature/boolean-algebraic-subtyping.md` (canonical post-implementation document) and `doc/whatif/completed/boolean-algebraic-subtyping.md` (archived design) for the complete design, and `doc/whatif/completed/parameterized-dict.md` for the Record/Map split implementation.

## Column Constraints — `RowTail::Uniform`

> **Implemented (S-842/S-843/S-856)**: `RowTail` enum and `Row.tail` field are in `src/type_def.rs`. The annotation parser for `{_ : V}` and `{_@K : V}` syntax is implemented in `src/typecheck_annot.rs` (S-843). `UNIFY-UNIFORM` is fully implemented in `type_unify.rs`: the Uniform+Uniform case unifies value types and validates named fields (T-1007); the Empty+Uniform case performs TypeVar-join or concrete-subtype validation for named fields from both rows (T-1024, implemented in S-856). `Arc<TyConDef>` pointer identity for cross-scope TyCon rejection is structurally in place (B-343, implemented in S-856); the operative cross-scope check becomes meaningful after T-1112 (`Type::TyCon(Arc<TyConDef>)` migration).

`RowTail::Uniform` is a deterministic constraint on the tail of a row — not a row variable. It expresses "whatever fields are present, their values have type V." This is distinct from BAS row variables (eliminated); `RowTail::Uniform` is a finite conjunction of field-type constraints that happen to be uniform.

```rust
pub enum RowTail {
    Empty,                              // closed record — default for all current Row constructions
    Uniform {                           // column constraint
        key: Option<Box<Type>>,         // None = {_ : V}; Some(K) = {_@K : V}
        value: Box<Type>,               // all present fields have this value type
    },
}
```

All existing `Row { fields }` constructions gain `tail: RowTail::Empty`. The `Uniform` variant is produced only when parsing `{_ : V}` or `{_@K : V}` annotation syntax.

**Syntax in annotation position:**

```tinct
config@{_ : String}               # all values String
counts@{_ : Int}                  # all values Int
mixed@{host: String  _ : Int}     # host is String; all other fields are Int
data@{_@String : Int}             # String keys, Int values
```

**User-defined column constraint types:**

```tinct
Map:     [type [let k@Equatable v]  [_@k : v]]   # typed-key uniform dict
Headers: [type [_ : String]]                      # all values String
Counter: [type [_ : Int]]                         # frequency/count dict
```

**Subtyping rules (validated by Nickel):**

```text
{f1:T1, ..., fn:Tn, Empty} <: {Uniform(None, V)}
    when all Ti <: V                              [S-ROW-CLOSED-TO-UNIFORM]

{Uniform(None, V1)} <: {Uniform(None, V2)}
    when V1 <: V2                                 [S-UNIFORM-COV]  (covariant in value)

{fi:Ti, Uniform(None, V1)} <: {Uniform(None, V2)}
    when Ti <: V2 and V1 <: V2                   [S-MIXED-TO-UNIFORM]

{Uniform(Some(K1), V1)} <: {Uniform(Some(K2), V2)}
    when K1 <: K2 and V1 <: V2                   [S-TYPED-KEY-UNIFORM]

{Uniform(Some(K), V)} <: {Uniform(None, V)}      [S-KEYED-TO-UNKEYED]  always
```

**Unification:** see `unify_rows` rules in [Type Inference](06-type-inference.md) §Unification. The substitution-first branching (apply `S` to `V` before branching on TypeVar vs concrete) is required to correctly handle already-bound TypeVars.

**Runtime:** `[@{_ : V} d]` wraps each field access in a guard thunk. Proxy contracts (Findler & Felleisen 2002) are applied on demand, preserving tinct's lazy evaluation guarantee. Typed-key enforcement (`{_@K : V}`) is compile-time only until T-921 (Key enum generalization) ships.

**`Map K V` as transparent alias.** `Map K V` expands to `{_@K : V}` — checked for `k@Equatable` constraint at compile time. `Map String Int` and `Map Int Int` are distinguished statically; until T-921, runtime does not check key types, only value types.

## User-Defined Type Constructors

User-defined type constructors add two `TypeNode` constructors, a unified `TyConDef` store, variance annotations, and nominal ADTs. These are the structural type system changes introduced by the user-type-constructors feature.

### TypeNode.TypeConstructor and TypeNode.TypeApplication

**`TypeNode.TypeConstructor { name: String }`** has two roles depending on normalization state:

- **Transient (pre-normalization)**: a bare type name encountered in a type-stage expression (e.g., looking up `Color` in the type-stage env). These are always eliminated by `expand_all_tycon_apps` before the type checker sees the result.
- **Leaf identity (post-normalization)**: a qualified constructor name containing `.` (e.g., `"Color.Red"`, `"Direction.North"`). These remain after normalization as the nominal identity markers of specific ADT constructors. `TypeConstructor "Color.Red" <: TypeConstructor "Direction.Red"` is false because the names differ — nominal identity is preserved through qualified names, not through type opacity.

**`TypeNode.TypeApplication { ctor: TypeNode  args: [Seq TypeNode] }`** is always transient. It exists during type-stage computation to represent an unapplied type constructor plus its arguments, but is always eliminated by `expand_all_tycon_apps` before the type checker sees the result. After normalization, the type checker works only with: primitives, Record, Union, Intersect, Arrow, Recursive, RecursiveRef, TypeVar, and qualified TypeConstructor leaves.

Builtin-opaque types (`Seq`, `Map`, `Handle`) are an exception: `expand_named` returns `TypeNode.TypeApplication(TypeConstructor(name), args)` for these without structural expansion, so `App(TyCon("Seq"), Int)` etc. remain in the type checker after normalization. The type checker's App arm handles them via UNIFY-TYCON (variance-directed comparison by TypeConstructor name).

### Variance Annotations

Type parameters declared in `[let ...]` carry variance annotations via `name@VarianceName`:

| Annotation | Variance | Subtyping rule |
|---|---|---|
| `a@Covariant` | Covariant | `F a <: F b` when `a <: b` |
| `a@Contravariant` | Contravariant | `F a <: F b` when `b <: a` |
| `a` (none) | Invariant | `F a <: F b` only when `a = b` |
| `a@Phantom` | Phantom | `F a <: F b` always |

Variance is inferred for transparent aliases via polarity analysis (Dolan 2017 §4) and stored in `TyConDef.variance: Vec<Variance>`. Explicit annotations serve as checked declarations — the inferred variance is compared against the declared variance and a type error is raised on conflict. TyCon kind is derived from `TyConDef.variance.len()` — no separate kind registry is needed.

`annotation_to_variance` maps variance names to `Rust Variance` variants in a closed 4-entry table (`"Covariant"`, `"Contravariant"`, `"Invariant"`, `"Phantom"`). The `Variance: [type Covariant Contravariant Invariant Phantom]` prelude declaration exists for reflection (`[describe Variance]` works) but does not power the dispatch.

### Nominal ADTs

A `[type ...]` declaration with uppercase constructor names creates a nominal ADT. The type declaration creates both:

1. A type registered in `TyConDef` with `constructors: Vec<(String, usize)>` — qualified tag and payload arity.
2. A dict value whose fields are the constructors (accessed via dot: `Color.Red`, `Result.Ok`).

Constructor tags are qualified: `"Color.Red"`, `"Result.Ok"`, `"Seq.Cons"`. Nominal identity is preserved through qualified names — two types sharing a bare constructor name (e.g., `Result.Ok` and `Validated.Ok`) are always distinguishable at runtime. `Value::Variant { tag: "Result.Ok" }` not `"Ok"`.

**`expand_named` for nominal ADTs** synthesizes `body` as `TypeNode.Union [TypeNode.TypeConstructor "Color.Red"  TypeNode.TypeConstructor "Color.Green" ...]` at declaration time. `TypeConstructor "Color.Red" <: TypeConstructor "Direction.Red"` is false by name inequality — nominal identity is preserved through constructor names, not through keeping `App(TyCon("Color"))` opaque.

**Exhaustiveness checking** uses `TyConDef.constructors` — the arity-only `Vec<(String, usize)>` is exactly the right level of detail for Maranget (2007) matrix decomposition. Field types are irrelevant to coverage; they matter only for type-checking bindings within pattern arms.

**Pattern syntax.** Dot-access pattern heads (`[Result.Ok v]`, `Color.Red:`) are syntactically assembled by the parser via `flatten_dot_access_to_tag` in `src/ast.rs`. Constructor patterns require fully qualified dot-access syntax — bare uppercase names (e.g., `[Ok v]`, `Red:`) are a type error. The type checker does not silently qualify bare Constructor names: the type-checker-local elaboration in `typecheck_match.rs` qualifies tags for coverage analysis only and the result is never persisted to the stored AST, so bare names fail at runtime when compared against qualified variant tags.

### Absent — First-Class Absence

`Absent: [type Absent]` is a unit nominal type declared in prelude. `Absent.Absent` (a zero-payload variant) represents "this thing is not present." This separates absence from `[]` (empty collection). Builtins that return a missing value (`get?`, `env`) return `Absent.Absent`. Pattern matching is the canonical narrowing form; `absent?` is a type-erasing `Unknown → Bool` predicate (same category as `null?`).

`[or Absent T]` is the structural optional type. `null?` and `absent?` are not interchangeable — `null?` checks for `[]`, `absent?` checks for `Absent.Absent`.

## TypeAssert Runtime Validation

Both static and runtime TypeAssert checks are structural. The evaluator validates values against the full resolved `Type`, not a type name string. Record fields are checked lazily via proxy contracts (Findler & Felleisen 2002), preserving tinct's lazy evaluation guarantees.

**Elaboration.** The type checker resolves TypeAssert annotations and embeds the resolved type directly in the AST (Dunfield & Krishnaswami 2021, §elaboration). This follows the standard bidirectional typing approach: the checking judgment produces an elaborated term where type information is explicit.

```text
Expr::TypeAssert { expr, annotation }
→ Expr::TypeAssert { expr, annotation, resolved_type: RefCell<Option<Type>> }
```

The parser initializes `resolved_type: None`. The type checker fills it in during `resolve_type_assert()` via `resolve_annotation()`, applying the current substitution to produce a fully-substituted concrete type. Type aliases are resolved at this stage — the evaluator never resolves aliases itself. If type checking is skipped (`--no-typecheck` mode), `resolved_type` remains `None` and the evaluator degrades gracefully (see below).

Because the resolved type is part of the AST, it is captured naturally by `Unevaluated` thunks (which store `expr + env`). No changes to thunk state, `eval()` signatures, or environment structure are required.

**Structural validation judgment.** A judgment `v ∈ τ` ("value v inhabits type τ") defines structural validation at runtime. For primitive types, validation is immediate. For records, validation uses proxy contracts — field types are checked lazily when accessed, not eagerly at the assertion site.

*Immediate rules* (checked at assertion time):

```text
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

v = Variant { tag: "Seq.Nil" } ∨ v = Variant { tag: "Seq.Cons", .. }
────────────────────────────────── [VM-SEQ]
v ∈ App(TyCon("Seq"), τ)    (tag-only check; element type not validated eagerly)

────────────────────────────────── [VM-VAR]
v ∈ α
```

*Proxy contract rule* (shape checked at assertion time, field types checked lazily):

```text
v = Dict(entries),
∀(fᵢ: τᵢ) ∈ fields.  fᵢ ∈ string_keys(entries),
entries' = { fᵢ ↦ guard(entries[fᵢ], τᵢ, [fᵢ], span) | (fᵢ: τᵢ) ∈ fields }
          ∪ { k ↦ entries[k] | k ∉ dom(fields) }
────────────────────────────────────────────────────── [VM-RECORD-PROXY]
[@τ v] ⟶ Dict(entries')
```

**BAS width subtyping.** The cardinality check `ρ = Closed ⟹ string_keys(entries) = dom(fields)` formerly present in [VM-RECORD-PROXY] was removed during the BAS (Bounded Annotation Subtyping) implementation. Under BAS, a value with MORE fields always satisfies an annotation with FEWER fields — extra fields are never an error. The `ρ` (row tail) distinction between `Open` and `Closed` is retained for static type inference but does not affect runtime validation: all records are treated as width-subtype-compatible regardless of their row tail. This makes the closed-record cardinality condition vacuously true at runtime.

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
| `App(TyCon("Seq"), τ)` | Tag only — "is sequence" | Immediate | Element type opaque; materializing all would diverge |
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

```text
If Γ ⊢ e ⇒ σ  and  σ <: τ  and  eval(e) = v  and  τ is deeply checkable,
then v ∈ τ.
```

A type τ is *deeply checkable* when all constituents are fully observable at runtime: primitives, singleton literals, records (recursively), and `Unknown`. The invariant holds because `is_subtype` is more restrictive than `v ∈ τ` for these types.

For *opaque* type constructors (`Fn`, `Seq`), the invariant degenerates to tag-level soundness: [VM-FN] and [VM-SEQ] perform only tag checks, so they accept values that `is_subtype` would reject (e.g., `Fn(Int→Int) ∈ Fn(String→String)` succeeds at runtime). The forward direction still holds: if `is_subtype(σ, τ)` passes statically, the tag check will certainly pass at runtime. But the converse does not — runtime tag success does not imply static subtyping.

**Error messages.** Runtime validation errors report the structural path to the mismatch:

```text
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

## Consistent Subtyping

Runtime TypeAssert validation uses **consistent subtyping** (`~<:`), the AGT (Abstracting Gradual Typing) relation that extends structural subtyping to handle `Unknown` at erased positions.

**Definition:** `value_matches_type(v, T)` is defined as `is_consistent_subtype(ground_type_of(v), T)`, where:

- `ground_type_of(v)` extracts the ground type of runtime value `v`, producing `Type::Unknown` at erased positions (Seq elements, Map values, Dict field values, Function params/returns)
- `is_consistent_subtype(A, B)` implements the AGT `~<:` relation (Garcia et al. 2016, Proposition 22)

**Key rules:**

```text
────────────────────────  [CS-UNKNOWN-L]
Unknown ~<: T

────────────────────────  [CS-UNKNOWN-R]
T ~<: Unknown

A <: B
────────────────────────  [CS-SUBTYPE]
A ~<: B
```

For compound types, consistent subtyping recurses structurally:

- **Seq:** `Seq(A) ~<: Seq(B)` iff `A ~<: B`
- **Map:** `Map[K1 V1] ~<: Map[K2 V2]` iff `K1 ~<: K2` and `V1 ~<: V2`
- **Record:** `{f₁: T₁, ...} ~<: {g₁: U₁, ...}` iff all required fields `gᵢ` exist in the first record with `Tᵢ ~<: Uᵢ` (width subtyping)
- **Function:** contravariant params, covariant return (same as `<:`)
- **Union/Intersection:** same as `<:` with recursive `~<:` checks

**Laziness preservation caveat:** Because `ground_type_of` must not force thunks, element types in Seq values and field types in Dict values are erased to `Unknown`. This means:

- `[@Seq[Int] [seq 1 "hello" 3]]` passes at runtime — element types are not checked
- `[@[name: String] [name: 42]]` passes shape validation (field `name` exists), but the field type is checked lazily via proxy contract wrapping when accessed

TypeAssert **Seq element validation is static-only**. The "lint passes = safe to run" guarantee applies to tag-level checks (is the value a Seq?) but not element type checks. The same applies to Dict field types — field presence is checked eagerly, field types are checked lazily.

**References.** Garcia, R., Clark, A.M. & Tanter, É. (2016). "Abstracting Gradual Typing." *POPL '16*, pp. 429-442. — Proposition 22 (Type Safety): the runtime check is the restriction of the static consistent subtyping relation to ground types.

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

```text
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

```text
// Before
type BuiltinFn = fn(args, named, depth, call_span) -> Result<Value, Box<EvalError>>;

// After
type BuiltinFn = fn(args, named, depth, call_span) -> Result<Rc<Thunk>, Box<EvalError>>;
```

Builtins that return materialized values wrap them in `Thunk::new_materialized()`. Builtins like `$map` and `$if` return thunks directly. This removes the eager materialization boundary that separates builtins from lazy evaluation.

**Rationale:** The `Rc<Thunk>` return type allows builtins to participate in lazy evaluation while maintaining backward compatibility (wrap in `Thunk::new_materialized()` for eager builtins). Operations like `$if` return the chosen branch as a thunk; `$map` returns a dict with lazy PendingCall values — neither requires eager materialization.

**Type inference is unchanged** — return types are determined by unifying the call signature during type checking, not by inspecting returned thunk contents. This change is a runtime optimization only.

**Performance trade-off:** Inherently materializing builtins (~60% of the 28 current builtins: arithmetic, string ops, comparisons) pay two extra heap allocations per call (Thunk + Rc wrapper) to wrap their `Value` result. For lazy-capable builtins (`$if`, `$merge`, `$map`, `$update`), this eliminates the eager materialization boundary. Net benefit when lazy operations dominate. If profiling shows the overhead is significant, a dual-signature approach (`EagerBuiltinFn` vs `LazyBuiltinFn`) could be considered.

## Equirecursive Types

Tinct supports equirecursive types — recursive type expressions that are compared structurally, without explicit `fold`/`unfold` operations. The type checker unfolds `TypeNode.Recursive` on demand during subtype checking; functions accept recursive types transparently.

### TypeNode Constructors for Recursive Types

Two `TypeNode` constructors represent equirecursive types:

**`TypeNode.Recursive { var: String  body: TypeNode }`** — a recursive type `μvar.body`. `var` is a globally unique gensym binder name (e.g. `"𝜇ꜱʏᴍ⧼lst⧽42"`), generated at construction time. `body` is a concrete TypeNode with `RecursiveRef(var)` at each recursive position — there is no deferred function stored, only a concrete TypeNode value. The var name is the sigma key used by S-Assum during subtype checking.

```tinct
# TypeNode.Recursive representing μlst.(Absent | {head: Int, tail: μlst})
TypeNode.Recursive {
  var:  "𝜇ꜱʏᴍ⧼lst⧽42"
  body: TypeNode.Union types: [
    TypeNode.Absent
    TypeNode.Record fields: {
      "head": TypeNode.Int
      "tail": TypeNode.RecursiveRef name: "𝜇ꜱʏᴍ⧼lst⧽42"
    }  open: false
  ]
}
```

**`TypeNode.RecursiveRef { name: String }`** — a back-reference inside a Recursive body. `name` matches the `.var` of the enclosing `Recursive`. `RecursiveRef` is a leaf: it has no `@Child` fields, so `typenode_map_children` passes it through unchanged and `Substitution::apply` leaves it untouched. It never appears outside a Recursive body in a well-formed type.

**`TypeNode.TypeVar { name: String  level: Int }`** — the leaf TypeNode constructor for inference variables. TypeVar is a TypeNode constructor, not a separate Rust variant; `walk_type` finds TypeVar nodes automatically via `TypeNode.children`. `name` is the fresh variable name (e.g. `"_t42"`); `level` is the Kiselyov creation-time level. See [Type Inference](06-type-inference.md) §CheckerType for the full specification.

### The `mu` Combinator

`mu` is a type-stage function in the prelude. It generates a fresh binder var, calls the body function eagerly with a `RecursiveRef` sentinel, and wraps the result in `TypeNode.Recursive`. No deferred `Fn` is stored:

```tinct
--- stage: type
[
  mu: [fn [let f]
    [let var  [gensym-with-scope "𝜇" "rec"]]       # → "𝜇ꜱʏᴍ⧼rec⧽N"
    [let body [f TypeNode.RecursiveRef name: var]]  # eager call: stores TypeNode, not Fn
    TypeNode.Recursive var: var  body: body]
]
```

Users can call `mu` inline wherever a TypeNode is expected:

```tinct
# Inline recursive type annotation
depth: [fn@Int [tree@[mu [fn [let self]
    [or Absent [record value: Int  left: self  right: self]]]]]]
  [if [absent? tree] 0 [+ 1 [max [depth tree.left] [depth tree.right]]]]

# Named alias — identical semantics, no explicit mu needed
BinTree: [type [or Absent [record value: Int  left: BinTree  right: BinTree]]]
```

Named aliases are the ergonomic form for repeated use. Named `self` (not `$_`) is used as the body parameter — `$_` desugaring binds at the nearest enclosing argument position, not at the `mu` boundary.

### Named Aliases and `expand_named`

Named recursive type aliases do not require explicit `mu`. The annotation resolver detects self-references via the expansion stack and wraps the body in `TypeNode.Recursive` automatically.

**`expand_named` algorithm:**

```text
expand_named(name, args, stack, env):
  decl = TypeEnv::lookup_type_decl(name)
  if decl is None: error(UndefinedType(name))
  if args.len() != decl.params.len(): error(ArityMismatch)

  # Early exit for zero-param types with no transient TypeConstructor references
  if decl.params.is_empty() and not body_contains_tycon_ref(decl.body):
    return CheckerType(Arc::clone(&decl.body))

  # Builtin-opaque types (Seq, Map, Handle) stay as App leaves
  if decl.is_builtin_opaque:
    return apply_args(TypeNode.TypeConstructor name: name, args)

  # Expansion stack cycle detection via Arc::ptr_eq
  tycon_arc = TypeEnv::lookup_tycon_def(name)
  if let Some(pre_name) = stack.get_pre_assigned_name_by_ptr(&tycon_arc):
    return TypeNode.RecursiveRef name: pre_name    # back-reference to cycle origin

  # Pre-assign binder name BEFORE expanding (needed for nested self-references)
  fresh_var = gensym_fresh('𝜇', name)             # e.g. "𝜇ꜱʏᴍ⧼IntList⧽42"
  stack.push(tycon_ptr, fresh_var)

  # Substitute type args, expand all TypeApplication/TypeConstructor references
  body_substituted = substitute_typenode(decl.body, zip(decl.params, args))
  expanded = expand_all_tycon_apps(body_substituted, stack, env)
  stack.pop()

  # Contractiveness check — rejects μa.a, μa.(a|Int), etc.
  if contains_recvar(expanded, fresh_var):
    if not is_contractive(expanded, fresh_var):
      error(TypeError(NonContractive))
    return TypeNode.Recursive { var: fresh_var, body: expanded }
  return expanded                                  # non-recursive alias: return body directly
```

Only the cycle-origin alias is wrapped in `TypeNode.Recursive`. For mutually recursive aliases (`EvenList` / `OddList`), the first alias encountered is the cycle origin; the second alias's body is inlined into the first's Recursive node.

**Normalization invariant.** All named types are expanded at annotation resolution time. No `TypeNode.TypeApplication` or bare `TypeNode.TypeConstructor` reaches the type checker. The type checker works only with: primitives, Record, Union, Intersect, Arrow, Recursive, RecursiveRef, TypeVar, and qualified TypeConstructor leaves.

### TypeVar as Leaf TypeNode Constructor

`TypeNode.TypeVar` participates in generic TypeNode traversal alongside all other constructors. Pure-traversal walkers use `walk_type` with predicates on the TypeNode variant tag — no explicit match arms needed:

- **`collect_type_vars`**: `walk_type` + `typenode_tag(t) == "TypeNode.TypeVar"` predicate. No explicit TypeVar arm; TypeVar nodes are found automatically because they are TypeNode values with `TypeNode.children` returning `[]` (leaf).
- **`has_inference_vars`**: same pattern — `walk_type` + TypeVar tag predicate.
- **`check_kind_wellformed`**: pure `walk_type`, no predicate on TypeVar specifically.

The walkers that require explicit Rust arms (semantically special cases only):

| Walker | Arms needed | Reason |
|--------|-------------|--------|
| `is_subtype_inner` | TypeVar, Recursive | TypeVar: gradual typing (TypeVar relates to everything, treated as Unknown); Recursive: S-Assum + S-Exp |
| `unify` | TypeVar, Recursive | TypeVar: bind via occurs check + subst; Recursive: 5-arm opening (see §Unification for Recursive Types in doc/06) |
| `Substitution::apply` | TypeVar only | One explicit arm: subst lookup by name; if unbound, return unchanged. All other TypeNode constructors — including Recursive, RecursiveRef, Union, Record, Arrow — are handled by `typenode_map_children` with a recursive apply call |
| `PartialEq` | Recursive only | Structural equality: same `.var` name AND same `body` TypeNode. This is sufficient given globally unique gensym `.var` names — two `Recursive` nodes with the same `.var` must be the same binder. TypeVar uses structural equality on `name` (level ignored per Kiselyov — levels are stored in `state.levels`, not in the equality relation) |

`collect_type_vars` reads level from `state.levels[name]`, not from the TypeNode `level` field. `state.levels` is the authoritative current level (updated by level lowering). DICT-GEN generalization checks `state.levels[name] > enclosing_level`.

---

## Archived: Rémy Row Polymorphism — See [doc/whatif/completed/remy-row-polymorphism.md](../whatif/completed/remy-row-polymorphism.md)
