# Rémy Row Polymorphism — Archived Design

> **Status: Completed (superseded).** This document records the Rémy-style kinded row-variable unification design that was implemented in the `row-unification-*` sprint series. The implementation was subsequently replaced by Boolean-Algebraic Subtyping (BAS), which handles record subtyping via the type lattice rather than row variable unification. See [doc/07-type-extensions.md §Boolean-Algebraic Subtyping](../07-type-extensions.md) for the live specification.

---

## Row-Variable Unification — Kinded Rémy Model

This design used kinded row-variable unification following Rémy (1994). Row variables were first-class participants in type inference with a separate **Row kind**, enabling the type checker to infer record extension and restriction through polymorphic function boundaries.

**Representation choice:** The Row type used a **dict+tail** representation (field map plus tail variable) rather than Rémy's cons-list (`Extend(l, τ, ρ)`). Rémy's left-commutativity equations (`l₁:τ₁ ; l₂:τ₂ ; ρ ≡ l₂:τ₂ ; l₁:τ₁ ; ρ`) make rows semantically unordered — the dict+tail representation computes directly in the quotient algebra of rows under these equations, representing each equivalence class as a single canonical form (unordered field map) rather than an arbitrary representative (ordered cons-list).

**Design rationale:** Rémy (1994) Theorem 4.7 proves principal type existence for the kinded row system. Wand (1987, Theorem 1, corrected 1988) proves completeness for the presence-only restriction (no absence flags). However, BAS (Dolan 2017, Parreaux 2020, Chau & Parreaux 2026) provides a more powerful framework where extensible records emerge from the Boolean algebra of types without needing separate row variables.

### Part 1: Row Kind

**Notation:** This section uses ρ for row variables, following Rémy (1994) and Wand (1987). The [Evaluation](../../08-evaluation.md) §Scope Chain Semantics section uses ρ for environments, following Launchbury (1993). The two uses are confined to separate sections and do not interact — the row-variable ρ participates in type inference, while the environment ρ participates in evaluation.

Rows were a **separate sort** from types. A row mapped labels to types with an optional tail variable. Under BAS, the `Row` struct is simplified to just fields — no tail:

```rust
// CURRENT (BAS): All records are closed — no tail variable
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub fields: HashMap<String, Type>,
}
```

**ARCHIVED (Rémy):** The following types no longer exist in the codebase:

```rust
// Row with extensible tail (Rémy row polymorphism)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub fields: HashMap<String, Type>,   // known fields {l₁: τ₁, l₂: τ₂, ...}
    pub tail: RowTail,                   // Empty (closed) or RowVar(ρ) (open)
}

enum RowTail {
    Empty,
    RowVar(String, u32),  // ρ — row variable (name, Kiselyov generalization level)
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

```text
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

```text
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

```text
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

> **Note:** Under BAS, record unification is handled via `is_subtype()` checks using Boolean algebra rules, not field partitioning. The row unification algorithm below is not active in the current implementation.

In the Rémy design, row unification was the core of the type system. It used **field partitioning** — given two rows, partition their fields into shared (present in both) and unique (present in only one), then unify shared field types and bind row variable tails to the other side's unique fields. This directly computed in the quotient algebra of Rémy's left-commutativity equations.

**Unification algorithm:**

```text
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

```text
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

```text
UNIFY-RECORD:
  unify_types(Record(r₁), Record(r₂), S) = unify_rows(r₁, r₂, S)
```

All record unification delegates to row unification.

**Complexity:** Field partitioning is O(n) where n is the total number of fields across both rows (hash-based set operations on HashMap keys). This improves on the cons-list extract-and-recurse approach which is O(n²) worst case (O(n) scan per field). For tinct's use case (configuration records, typically < 100 fields) both are acceptable, but O(n) is strictly better.

### Part 4: Instantiation and Generalization

Row variables participate in generalization and instantiation via the standard HM mechanism, extended to two kinds.

**Variable collection** (two sets):

```text
collect_type_vars(τ) → Set<String>     # type variables in τ
collect_row_vars(τ) → Set<String>      # row variables in τ

collect_row_vars(Record(Row { fields, tail })):
  row_vars_in_fields(fields) ∪ row_vars_in_tail(tail)

row_vars_in_fields(fields) = ⋃{ collect_row_vars(τ) for τ in fields.values() }
row_vars_in_tail(RowVar(r)) = {r}
row_vars_in_tail(Empty)     = {}
```

**Instantiation** freshens both namespaces independently:

```text
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

**Generalization** (with levels, per [Type Inference](../../06-type-inference.md) §Let-Generalization): In the Rémy design, row variables carried levels identically to type variables. Under BAS, `TypeScheme` carries only type variables:

```rust
pub struct TypeScheme {
    pub type_vars: Vec<String>,    // universally quantified type variables
    // row_vars removed — BAS uses closed records, no row variable generalization
    pub body: Type,
}
```

Generalization now operates only on type variables. Record width subtyping is handled via BAS intersection/union rules, not row polymorphism.

### Part 5: Access Chain Constraint Generation

> **Note:** Under BAS, access chains do not generate row variable constraints. Width subtyping is handled via BAS rules. The constraint generation described below is not active in the current implementation.

In the Rémy design, row variables enabled constraint generation for access chains instead of falling back to `Unknown`.

```text
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

Under BAS, width subtyping replaces row variable constraints for access chains (see [doc/07-type-extensions.md §Boolean-Algebraic Subtyping](../07-type-extensions.md)).

### Part 6: Subtyping

`is_subtype` handles `Record(Row)` directly using the field map:

```text
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

This preserves the current behavior ([Type Inference](../../06-type-inference.md) §Subtyping S-REC) while working with the new Row representation. The `RowVar` in subtyping position acts as `Open` — consistent with the gradual typing design where unknown row extensions are permitted.

### Part 7: Display

> **Note:** Under BAS, all records are closed. The `tail` field is always `Empty`, and row variable display logic is unused. Record types display as field lists only.

In the BAS era, record types display using field-only syntax (no tail):

```text
Display for Record(IndexMap<String, Type>):
  field_strs = ["{l}: {τ}" for (l, τ) in fields]
  return "[" + field_strs.join("  ") + "]"
```

Examples (BAS era):

- `Record({name: Str, age: Int})` → `[name: Str  age: Int]`
- `Record({})` → `[]`

Under BAS, record types display using field-only syntax (no row variable tail).

### Part 8: Migration Reference

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

**Structural notes.** The `Row { fields, tail }` structure is analogous to `(IndexMap<String, Type>, RowRest)`: `fields` is the same HashMap, and `tail` is a `RowTail` distinguishing closed (`RowTail::Closed`), open-with-variable (`RowTail::RowVar(ρ)`), and empty (`RowTail::Empty`) rows. The `RowVar` form names the openness explicitly, enabling the kind-separated substitution described below.

**Substitution split.** The unification function routes variable bindings to the correct map based on the variable's kind (inferred from context: `TypeVar(α)` → `type_map`, `RowTail::RowVar(ρ)` → `row_map`). Type variables and row variables occupy separate namespaces enforced by the `Substitution` structure.

**Construction.** Inline struct construction is used in the implementation (e.g., `Row { fields: HashMap::new(), tail: RowTail::Empty }`). Helper functions like `Row::closed()` or `Row::var()` were not added.

### Part 9: Properties

**P1 — Principal types.** Every well-typed expression has a principal type under the kinded row unification algorithm. For the presence-only restriction (no absence flags), this follows from Wand (1987, Theorem 1, corrected 1988). The full system with presence/absence flags is covered by Rémy (1994, Theorem 4.7). The dict+tail representation computes in the quotient algebra of Rémy's rows under left-commutativity; since it is isomorphic to the cons-list representation, the principal type theorem applies unchanged.

**P2 — Kind safety.** Type variables and row variables inhabit separate namespaces enforced by the `Substitution` structure (`type_map` vs `row_map`) and by Rust's type system (`Type` vs `Row` are distinct types). A type variable α can never be bound to a row, and a row variable ρ can never be bound to a type. This prevents the class of bugs exemplified by Elm issue #656.

**P3 — Row commutativity.** `{a: Int, b: Str, ...ρ}` unifies with `{b: Str, a: Int, ...ρ}` — field order in rows is irrelevant. This is enforced structurally by the dict+tail representation: the `HashMap` is an unordered field collection, so commutativity is automatic rather than computed via extraction.

**P4 — Occurs check termination.** The per-kind occurs check prevents infinite types (`α = Record({x: α})`) and infinite rows (`ρ = {x: Int, ...ρ}`). The row-variable occurs check traverses field types to prevent infinite structures through nesting (`ρ = {x: Record({y: Int, ...ρ})}`). Combined with the finite-depth property of tinct's AST, unification terminates.

**P5 — Type language stability.** The type language visible to users is unchanged by the internal representation. Programs using open-record annotations infer more precise types under row polymorphism than they would under `Unknown` fallback — this is strictly more informative.

**P6 — Forward compatibility with full Rémy.** Adding presence/absence flags changes field map values from `Type` to `(FieldPresence, Type)`. The partitioning algorithm gains a presence-compatibility check (Present must match Present, Absent must match Absent), and field access must skip Absent fields. The overall structure (partition shared/unique, unify shared, bind tails) is preserved. See Part 1: Row Kind for the extension point.

**P7 — Label uniqueness.** The `HashMap<String, Type>` structurally prevents duplicate labels in any row. This invariant is maintained through all operations: construction (from source), unification (partitioning preserves uniqueness), and substitution application (field merging of disjoint maps). No runtime duplicate-label check is needed.

**P8 — Tail-field disjointness.** The fields of a row and the fields of its resolved tail are disjoint at unification time, not after full substitution resolution. When `unify_remainders` binds a tail `ρ` to `Row { fields: U, tail: t }`, the unique fields `U` were computed as the set difference `F_other \ shared` — fields present in the other row but not in the row containing `ρ`. Since `ρ` is the tail of the row that contributed the `shared` fields, and `U` contains only fields *not* in that row, the two sets are disjoint at binding time. However, later unifications may bind row variables in `t`, surfacing new fields that overlap with the row's explicit fields. The implementation handles this via re-resolution and re-partitioning (Steps 3.5 and 3.6 in `unify_rows`), ensuring that overlapping fields are unified as shared fields before passing the truly disjoint remainders to `unify_remainders`.

### Part 10: Formal References

See [doc/17-references.md §Row polymorphism](../../17-references.md) for full citations of Rémy (1994), Wand (1987), Gaster & Jones (1996), Harper & Pierce (1991), and Bernstein (2024).
