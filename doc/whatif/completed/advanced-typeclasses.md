# What If: Advanced Typeclass Extensions for tinct

**State:** Accepted — 2026-05-14

What would it take to make tinct's typeclass system fully expressive — letting
user-defined types participate in primitive operators, express constraints over
record fields, and type mixed-mode arithmetic precisely?

## Current State

The HKT typeclass hierarchy provides Functor, Applicative, Monad, Foldable,
Traversable, Mappable, and Appendable as fully user-extensible classes. Three
areas remain as hardcoded limitations of the baseline system.

**Numeric requires multi-parameter type classes.** The four arithmetic operators
(`+`, `-`, `*`, `/`) are registered with a single-parameter constraint:

```
+ : Numeric a => a → a → a
```

This forces both arguments and the result to have the same type `a`. Mixed-mode
arithmetic is imprecise: `[+ 1 2.0]` should have type `Float` (Int widened to
Float before addition), but the single-parameter scheme cannot express this —
the return type is fixed to the same TypeVar as both arguments. `Numeric` stays
hardcoded because expressing `Int + Float → Float` requires three type
parameters and a functional dependency.

```tinct
# Should be: + : Add a b c | (a,b)→c => a → b → c
[+ 1 2.0]      # inferred as : Number (imprecise — should be Float)
[+ 1.5 2]      # inferred as : Number (imprecise — should be Float)
```

**Row-level constraints cannot be expressed.** A function that requires all
fields of a record to satisfy a constraint — "this record is equatable" — has
no syntax. The constraint system applies constraints to individual type variables,
not to records or their field sets.

```tinct
# Desired: function that deep-compares any record whose fields are all Equatable
# No way to express this constraint today — must use Unknown or explicit field list
deep-eq: [fn@[return: Bool  constraint: [???]] [a b] ...]
```

**User-defined types cannot participate in primitive operators.** `[=]`, `[<]`,
and `[str]` dispatch via hardcoded Rust type inspection — `value.type_name()` is
matched against a known set of primitive types. A user-defined nominal type
`MyPoint` cannot be compared with `[= p1 p2]` or displayed with `[str p]` even
if the user writes an Equatable or Showable instance.

```tinct
[MyPoint: [type [x@Int y@Int]]]

[EquatableMyPoint: [instance [Equatable MyPoint]
  [=: [fn [a b] [and [= a.x b.x] [= a.y b.y]]]]]]

[= point-a point-b]   # type error — Equatable for MyPoint not reached at runtime
```

### What's Missing

1. A 3-parameter `Add a b c | (a,b) → c` class expressing numeric coercion precisely.
2. Constraint propagation rules that distribute over BAS record intersections.
3. Runtime dispatch routing of primitive operators through user-defined instance dicts.

## Why These Extensions Matter for tinct

**Mixed-mode arithmetic is common in data processing.** Any pipeline that
mixes integer counts with floating-point rates produces `[+ n rate]` calls.
With precise MPTC typing, the return type is known statically and flows through
the rest of the pipeline without requiring `Unknown`-escaping annotations.

**Record equality and display are fundamental.** A language that positions itself
for data processing must support structural equality and display for user-defined
data types. `[= record-a record-b]` should work for any record whose fields are
equatable — and it should work automatically, without requiring the user to
enumerate every field manually.

**Instance dicts are already first-class.** The monad `[do result ...]` form
already passes instance dicts at runtime. Extending this pattern to primitive
operators makes the system uniform: every operator can be overloaded by writing
an instance, exactly as users already write Functor or Monad instances.

## Design

### Multi-Parameter Type Classes and Functional Dependencies

#### Functional Dependency Resolution

`Add a b c` constraints propagate as **residual constraints** in `TypeScheme.constraints`
(Jones 1995 qualified types). Resolution fires via **improvement** (Jones 2000, §5–6):
whenever the determining positions (`a`, `b`) become ground by unification, the
type checker looks up the matching `Add` instance and unifies `c` with the instance's
result type. This is eager — it fires during `check_constraints` whenever new bindings
are made, not deferred to call sites.

Consequence: `[fn [x y] [+ x y]]` infers `Add a b c => Fn@c [a b]` without annotation.
At each call site, `a` and `b` are instantiated to concrete types, improvement fires,
and `c` is resolved. If both determining vars are ground at generalization but no
instance matches, the type checker rejects immediately (GHC §9.4.1 refinement).
No defaulting rule or monomorphism restriction is needed — tinct's instance table
is closed.

Implementation: improvement fires in `check_constraints_on_var` when a TypeVar
bound to a determining position becomes ground. It checks whether all determining
positions of any pending FD constraint are now ground, looks up the instance, and
binds the determined position(s). This is the same pass that already fires constraint
satisfaction checks — no new infrastructure required beyond the MPTC constraint form.

References: Jones (2000) §5–6 (improving substitutions); Sulzmann et al. (2007)
(CHR confirmation: rules fire when guard is satisfied, otherwise retained).

The `Constraint` enum gains support for multi-parameter bindings:

```rust
pub enum Constraint {
    Class {
        class: String,
        vars: Vec<String>,          // multiple type variable names
        fundeps: Vec<(Vec<usize>, Vec<usize>)>, // (determining, determined) index sets
    },
    HasField { label: Label, dict_var: String, field_var: String },
}
```

The arithmetic operators are re-typed using a 3-parameter `Add` class with
functional dependency `(a, b) → c` — given the types of both operands, the
result type is uniquely determined (Jones 1994):

```
Add a b c | (a,b) → c

instances:
  Add Int    Int    Int    ← homogeneous int
  Add Float  Float  Float  ← homogeneous float
  Add Int    Float  Float  ← Int widened to Float
  Add Float  Int    Float  ← Int widened to Float
  Add Number Number Number ← gradual numeric
  Add Number Int    Number ← gradual + int
  Add Int    Number Number ← int + gradual
  Add Number Float  Number ← gradual + float
  Add Float  Number Number ← float + gradual
```

The Number-involving instances preserve Number as the result type (rather than widening to Float) because Number is the gradual numeric type — a Number-typed value could be Int or Float at runtime, so the result must remain Number to preserve soundness. Widening to Float would incorrectly discard the possibility that both operands are Int.

The arithmetic primitives are re-registered:

```
+ : Add a b c => a → b → c
- : Sub a b c => a → b → c
* : Mul a b c => a → b → c
/ : Div a b c => a → b → c
```

Constraint resolution with a functional dependency: when the type checker sees
`[+ x y]` and resolves `x : Int` and `y : Float`, it looks up the `Add Int Float`
instance, determines `c = Float` from the functional dependency, and binds the
result TypeVar to `Float`. The inference is deterministic: no backtracking, no
ambiguity.

```tinct
[+ 1 2.0]      # Add Int Float Float → result : Float  ✓
[+ 1.5 2]      # Add Float Int Float → result : Float  ✓
[+ 1 2]        # Add Int Int Int → result : Int  ✓
[+ 1.5 2.5]    # Add Float Float Float → result : Float  ✓
```

**User-defined arithmetic types.** A user-defined numeric type (say, a
`Decimal` wrapper for exact arithmetic) can participate in `[+]` by declaring
an `Add Decimal Decimal Decimal` instance with a runtime method — once runtime
dispatch (below) is in place.

```tinct
[AddDecimalDecimal: [instance [Add Decimal Decimal Decimal]
  [+: decimal-add]
  [-: decimal-sub]
  [*: decimal-mul]
  [/: decimal-div]]]
```

**Display format.** Multi-parameter constraints display as `Add a b c =>`:

```
Add Int Float c => Fn@c [Int Float]
```

After functional dependency resolution, `c = Float`:

```
Fn@Float [Int Float]
```

### Row-Level Constraint Propagation via BAS Intersection

Under BAS, every multi-field record is an intersection of single-field records:

```
{name: Str, age: Int} = {name: Str} & {age: Int}
```

Constraint propagation distributes over BAS intersection and union naturally, adding
two propagation rules to `check_constraints`:

```
[CONSTRAIN-FIELD]   C({f: τ}) ⊢ satisfied    iff    C(τ) ⊢ satisfied
[CONSTRAIN-INTER]   C(τ₁ & τ₂) ⊢ satisfied  iff    C(τ₁) ⊢ satisfied ∧ C(τ₂) ⊢ satisfied
[CONSTRAIN-UNION]   C(τ₁ | τ₂) ⊢ satisfied  iff    C(τ₁) ⊢ satisfied ∧ C(τ₂) ⊢ satisfied
[CONSTRAIN-TOP]     Showable(⊤) ⊢ satisfied
                    C(⊤) ⊢ error   for C ∈ {Equatable, Comparable, Numeric, Mappable, Appendable}
[CONSTRAIN-UNKNOWN] C(?) ⊢ satisfied             (AGT existential — deferred to runtime ClassEnv)
[CONSTRAIN-NEVER]   C(⊥) ⊢ satisfied                (⊥ is uninhabited — vacuously true)
```

**[CONSTRAIN-NEVER]:** `Never` (⊥) is the uninhabited type — no value has type `Never`.
Constraint satisfaction is vacuous: there is no value to violate the constraint.
This is consistent with `S-BOT`: `⊥ <: τ` for all `τ`, so `⊥` is a member of every
class's instance set by vacuity. No runtime check is ever needed.

**[CONSTRAIN-FIELD] restriction:** `C({f: τ}) iff C(τ)` assumes the class has a
compositional/structural interpretation — i.e., satisfaction of the field types
implies satisfaction of the whole record. This holds for all built-in classes
(`Equatable`, `Comparable`, `Showable`, `Numeric`). For user-defined classes with
non-structural semantics, the rule must not propagate automatically. Restrict
automatic field propagation to built-in classes until a declarative opt-in
mechanism is designed.

**[CONSTRAIN-UNION] direction:** The `∧` (AND) in `[CONSTRAIN-UNION]` is
intentional — ALL members must satisfy the constraint, not ANY. A union-typed
value could be either alternative at runtime, so both branches must support the
operation. Implementors: use `all()`, not `any()`.

**[CONSTRAIN-TOP] and [CONSTRAIN-UNKNOWN]:** The rules follow from AGT existential
lifting (Garcia, Clark & Tanter, POPL 2016): `C(A) = ∃t ∈ γ(A). C(t)`.

- `γ(⊤) = {⊤}` — Top concretizes only to itself. Since Top is not a member of
  any class's instance set, `C(⊤)` fails for all classes except `Showable`.
  `Showable(⊤)` succeeds because `Showable` is a total function by policy
  (`!matches!(ty, Error)` in `satisfies_constraint`). **Current code is
  correct — Top falls through to `false` for non-Showable classes by the
  existing allowlists.** No code change needed for Top.

- `γ(?) = STypes` — Unknown concretizes to the set of all static types.
  `C(?) = ∃t ∈ STypes. C(t)` holds for any non-empty class. `Equatable(?)`
  succeeds because `Equatable(Int)` holds; the runtime ClassEnv dispatch
  provides the actual check at the gradual boundary. **Code change required:**
  `satisfies_constraint` returns `false` for `Unknown` by accident (it is not
  in any allowlist); an explicit `Type::Unknown => return true` pre-check is
  needed so that `[CONSTRAIN-FIELD]` propagation through `Unknown`-typed fields
  gives the AGT-correct answer. **This fix is already applied** in `src/type_unify.rs`
  (the `if matches!(ty, Type::Unknown) { return true; }` pre-check at line 25)
  and is independent of the rest of this proposal.

References: Garcia, Clark & Tanter (POPL 2016); Castagna & Lanvin (ICFP 2017).

These rules apply when the constrained type variable is unified with a Record
type during constraint checking. The propagation is automatic — no new
annotation syntax is needed. A function annotated `constraint: [a: Equatable]`
accepts any type `a` satisfying Equatable, including records:

```tinct
# deep-eq accepts any two values whose shared type is Equatable
deep-eq: [fn@[return: Bool  constraint: [a: Equatable]] [x@a y@a]
  [= x y]]

# Works for primitives
[deep-eq 42 42]                             # Equatable Int ✓
[deep-eq "hello" "world"]                   # Equatable Str ✓

# Works for records — Equatable propagates over BAS intersection
[deep-eq {name: "Alice" age: 30}            # Equatable({name:Str}&{age:Int})
         {name: "Alice" age: 30}]           # = Equatable(Str) ∧ Equatable(Int) ✓

# Fails for non-equatable fields
[deep-eq {f: [fn [x] x]}                   # Equatable({f:Fn}) → Equatable(Fn) ✗
         {f: [fn [x] x]}]                   # type error: Fn is not Equatable
```

**Union constraint propagation.** For a union type `A | B`, `Equatable(A|B)`
requires that both `A` and `B` are equatable — because at runtime the value
could be either. This matches semantic intuition: you can only compare two
values of a union type if you can compare values of each alternative.

**BAS efficiency.** Because BAS records are already represented as intersections
in the type algebra, [CONSTRAIN-INTER] fires during the existing constraint
checking pass — no separate record-walking logic is needed. The propagation
follows the existing BAS normalization structure.

**Normalization ordering:** BAS normalization must complete before constraint
propagation fires. [CONSTRAIN-INTER] can only decompose a type already in
intersection normal form. If constraint checking runs on a pre-normalized
multi-field record, the match arms for `Intersection` do not fire. The
implementation must ensure `satisfies_constraint` is called on normalized types.

### Runtime Dispatch via ClassEnv Lookup

The evaluator is extended with a **ClassEnv dispatch table** — a global registry
mapping `(class_name, type_tag) → instance_dict`. When a primitive operator is
called on a value whose type is not a built-in primitive, the evaluator consults
the ClassEnv before falling back to the hardcoded Rust dispatch:

```rust
fn dispatch_eq(v1: &Value, v2: &Value, class_env: &ClassEnv) -> Result<bool> {
    if let Some(inst) = class_env.lookup("Equatable", v1.type_tag()) {
        // User-defined instance: call inst.= method
        let eq_fn = inst.get_method("=");
        call(eq_fn, [v1, v2])
    } else {
        // Primitive fallback: hardcoded Rust dispatch
        primitive_eq(v1, v2)
    }
}
```

**Instance registration.** When an `[instance ...]` declaration is evaluated
for a class that overloads a primitive operator (Equatable, Comparable, Showable,
Numeric/Add/Sub/Mul/Div), the evaluator registers the instance in the ClassEnv:

```tinct
[EquatableMyPoint: [instance [Equatable MyPoint]
  [=:   [fn [a b] [and [= a.x b.x] [= a.y b.y]]]]
  [not=: [fn [a b] [$not [= a b]]]]]]

# After evaluating this instance declaration, ClassEnv contains:
# "Equatable" × "MyPoint" → EquatableMyPoint dict
```

**Type-driven elaboration for known types.** When the type checker resolves
the constraint at a call site to a specific instance, it annotates the call with
the instance name. The evaluator uses this annotation for direct dispatch without
ClassEnv lookup:

```tinct
[= point-a point-b]
# Type checker resolves: Equatable MyPoint → EquatableMyPoint
# Evaluator sees elaborated hint: dispatch via EquatableMyPoint.=
```

For unresolved types (TypeVar, Unknown), the ClassEnv runtime lookup is the
fallback — dynamic dispatch for gradual typing boundaries.

**Primitive types remain fast.** The existing Rust dispatch for Int, Float, Str,
Bool, Number, Null is unchanged — the ClassEnv check short-circuits immediately
for known primitive type tags. No overhead is introduced for the common case.

### The Unified Picture

Together, the three extensions make the operator system fully open:

```tinct
# Mixed-mode arithmetic: precise return type
budget-rate: [+ 1000 2.5]    # : Float (Add Int Float Float)

# Record equality: automatic field propagation
[type Config [host@Str port@Int debug@Bool]]
config-a: [Config "localhost" 8080 true]
config-b: [Config "localhost" 8080 true]
[= config-a config-b]         # : Bool (Equatable propagates over Config's fields)

# User type participates in str
[type Color [r@Int g@Int b@Int]]
[ShowableColor: [instance [Showable Color]
  [str: [fn [c] [str "rgb(" c.r "," c.g "," c.b ")"]]]]]
red: [Color 255 0 0]
[str red]                     # "rgb(255,0,0)"  ✓

# User type in comparison
[type Priority [level@Int]]
[ComparablePriority: [instance [Comparable Priority]
  [<: [fn [a b] [< a.level b.level]]]]]
tasks: [[Priority 1] [Priority 3] [Priority 2]]
[sorted tasks]                # [[Priority 1] [Priority 2] [Priority 3]]  ✓
```

### Interaction with HKT

The MPTCs and runtime dispatch interact cleanly with the HKT typeclass hierarchy:

- `Equatable` and `Comparable` remain kind-`*` classes — single-type-variable, no HKT dependency.
- `Add`/`Sub`/`Mul`/`Div` are MPTC (3 type variables) but still kind-`*` on each parameter.
- The ClassEnv used for runtime dispatch is the same `ClassEnv` that stores Functor/Monad instances — one unified registry.
- Constraint entailment: `Comparable a` entails `Equatable a` — if a user declares `ComparablePriority`, `EquatablePriority` is derived automatically from the superclass relationship. Same-arity superclass entailment is supported via the existing `entails()` function (`src/type_unify.rs:73–96`).
- **Cross-arity entailment is not supported.** `Add a b c` does not automatically entail `Numeric a` or `Numeric b`. No position-mapping syntax exists for cross-arity superclass declarations, and none is needed: `Add`'s closed instance set already restricts operands to numeric types — any type without an `Add` instance produces a type error at the call site, which is the correct enforcement mechanism. Cross-arity entailment would require explicit position-mapping declarations (e.g. `superclass Add[0] Numeric`) which is out of scope.

### Limitations

**Overlapping instances are rejected.** For single-parameter classes, two
instances for the same `(class, type)` pair are a coherence violation. For
MPTCs with functional dependencies, the check is per **determining-position
tuple**: two `Add` instances with the same `(a, b)` pair but different `c` are
a coherence violation, regardless of `c`. The type checker rejects them. This
ensures dispatch is always deterministic (Sulzmann et al. 2007).

**Functional dependency coverage must be complete.** Every combination of `(a, b)` used with `[+]` must have a matching `Add a b c` instance. Using `[+]` on a custom type without a registered instance produces a type error at the call site, not a runtime error.

**Late dispatch for gradual typing boundaries.** When a value crosses a gradual typing boundary (annotated as `Unknown`) and is then compared with `[=]`, the ClassEnv lookup fires at runtime. If no instance is registered for the value's runtime type, the primitive fallback runs. This may silently succeed (if the value is a Dict and primitive `$=` handles it) rather than raising a type error — the expected gradual typing behavior.

## What Would Change

### `src/types.rs` — Constraint enum

**Current:** `Constraint { class: String, var: String }` (single TypeVar per constraint).
**Proposed:** `Constraint::Class { class: String, vars: Vec<String>, fundeps: Vec<(Vec<usize>, Vec<usize>)> }`. Existing single-var constraints migrate to `vars: vec![var]` with no functional dependencies.
**Impact:** Moderate. All Constraint construction sites must be updated. Constraint checking in `satisfies_constraint` is extended for multi-var lookup.

### `src/type_unify.rs` — Functional dependency resolution

**Current:** No MPTC infrastructure.
**Proposed:** After unification binds the "determining" TypeVars in a functional dependency, look up the matching instance to determine the "determined" TypeVar(s). Bind them in the substitution. This fires during `check_constraints` after argument types are resolved.
**Impact:** Moderate. New resolution step in the constraint checking pass.

### `src/type_unify.rs` — Row-level constraint propagation

**Current:** `satisfies_constraint` matches class name against known primitive instance sets.
**Proposed:** Add propagation arms for Record, Union, and Intersection types: decompose into constituent types and check constraint satisfaction recursively via [CONSTRAIN-FIELD], [CONSTRAIN-INTER], [CONSTRAIN-UNION].
**Impact:** Minor. A few additional match arms in `satisfies_constraint`.

### `src/eval.rs` — ClassEnv runtime dispatch

**Current:** `dispatch_eq`, `dispatch_lt`, `dispatch_str` use hardcoded Rust match on `value.type_name()`.
**Proposed:** Before the hardcoded match, check `class_env.lookup(class_name, value.type_tag())`. If an instance is found, call its method. Otherwise fall through to existing Rust dispatch.
**Impact:** Minor for primitive types (ClassEnv miss is a fast path). Moderate to implement: ClassEnv must be threaded through evaluation context (`EvalContext`).

### `src/eval.rs` — Instance registration

**Current:** `[instance ...]` declarations produce a dict value bound in the environment.
**Proposed:** Additionally register the instance in `EvalContext.class_env` when the declared class overloads a primitive operator.
**Impact:** Minor. A few extra lines in instance evaluation.

### `doc/06-type-inference.md` — §Primitive Built-in Constraints

**Current:** Documents 4 hardcoded single-parameter classes; notes Numeric stays hardcoded.
**Proposed:** Update `+`/`-`/`*`/`/` signatures to show MPTC form; remove the "Numeric stays hardcoded" limitation; document [CONSTRAIN-FIELD/INTER/UNION] propagation rules; document runtime dispatch extension.
**Impact:** Minor. Documentation update.

## Prerequisites

**Independent of `hkt-mappable-appendable` (can sprint now):**
- `[CONSTRAIN-UNKNOWN]` fix in `satisfies_constraint` — already applied (`src/type_unify.rs:23`)
- `[CONSTRAIN-FIELD/INTER/UNION/TOP/NEVER]` row-level propagation rules — new match arms in `satisfies_constraint`; no HKT dependency

**Requires `hkt-mappable-appendable`:**
- MPTC `Constraint` enum extension (`vars: Vec<String>`, `fundeps`) — requires the `ClassEnv` and `[class ...]`/`[instance ...]` infrastructure to be complete before MPTC instances can be declared
- Functional dependency improvement mechanism — builds on the multi-var Constraint form
- `Add`/`Sub`/`Mul`/`Div` re-registration as MPTC — requires both the Constraint extension and class infrastructure
- Runtime ClassEnv dispatch — requires instance registration infrastructure

## References

- Castagna, G. & Lanvin, V. (2017). "Gradual typing with union and intersection types." *ICFP '17*, pp. 1-28. ACM. — [set-theoretic treatment of ⊤ vs ? in gradual systems; confirms C(⊤) requires a literal instance while C(?) is existential]
- Garcia, R., Clark, A.M. & Tanter, É. (2016). "Abstracting gradual typing." *POPL '16*, pp. 429-442. ACM. — [AGT existential lifting: C(?) = ∃t ∈ γ(?). C(t); the formal basis for [CONSTRAIN-UNKNOWN]]
- Gaster, B.R. & Jones, M.P. (1996). "A polymorphic type system for extensible records and variants." Technical Report NOTTCS-TR-96-3. — [first-class labels and row-level constraints; BAS intersection propagation is a closed-record analogue]
- Jones, M.P. (1994). "A theory of qualified types." *Science of Computer Programming*, 22(3), 231-256. — [qualified types with multi-parameter constraints and functional dependencies; the formal model for Add a b c | (a,b)→c]
- Jones, M.P. (1995). *Qualified Types: Theory and Practice.* Cambridge University Press. — [dictionary translation; constraint satisfaction; instance coherence]
- Jones, M.P. (2000). "Type classes with functional dependencies." *ESOP 2000*, LNCS 1782. — [the functional dependency extension that makes MPTC inference decidable; (a,b)→c for Add]
- Peyton Jones, S., Jones, M. & Meijer, E. (1997). "Type classes: an exploration of the design space." *Haskell Workshop*. — [design tensions in MPTC: ambiguity, coverage, coherence; informs tinct's overlap-rejection policy]
- Sulzmann, M., Duck, G.J., Peyton Jones, S. & Stuckey, P.J. (2007). "Understanding functional dependencies via Constraint Handling Rules." *JFP*, 17(1), 83-137. — [CHR-based formal semantics for functional dependencies; confluence and termination proofs; the formal justification for tinct's coherence check]
- Wadler, P. & Blott, S. (1989). "How to make ad-hoc polymorphism less ad hoc." *POPL '89*, pp. 60-76. ACM. — [original typeclass paper; dictionary translation; the mechanism tinct's runtime dispatch adapts]
