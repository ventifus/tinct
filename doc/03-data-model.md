# Data Model

## Dicts Are Fundamental

The lowest-level unit is the dictionary (key-value pairs), not the list. First-class key-value pair syntax is core to the language.

A list is equivalent to a dict with integer keys:

```tinct
[a b c]  ≡  [0: a  1: b  2: c]
```

**Why this design:**
- **Unification** — One fundamental data structure. Functions like `map`, `filter`, `get` work uniformly on all data.
- **Flexibility** — Mixed integer and string keys naturally supported. Natural extension to keyword arguments.
- **First-class key-value pairs** — Matches the configuration language use case. Keys are names, not duplicated strings.

**Implementation:** May use different internal representations (dense vector for list-like data, HashMap for sparse/string keys) as a transparent performance optimization. Users never see the difference.

**Type-theoretic implication:** The static `Record` type tracks only string-keyed fields; integer-keyed (positional) entries are not part of the record type. A dict `[a b c  name: Alice]` has record type `[name: String]` — the positional entries `a`, `b`, `c` are invisible to the type checker. This is a deliberate consequence of unifying lists and records: positional entries are list-like data without static field names, while named entries form the record structure that type inference reasons about.

## One Bracket, One Structure

**`[]` is the only bracket type.** There is one syntax for the one fundamental data structure. Entries with `key:` are keyed; entries without get auto-incrementing integer keys. Both can appear in the same `[]`.

```tinct
[name: Alice  age: 30]          # All keyed — a "dict"
[a b c]                         # All auto-indexed — a "list" = [0: a  1: b  2: c]
[call $f $x timeout: 60]        # Mixed — positional + named
[]                              # Empty — list and dict are identical
```

**Parsing rule:** After parsing an entry, look ahead for `:`. If found, the entry is a key and the next thing is its value. If not, the entry is auto-indexed. The integer counter only increments for unkeyed entries — keyed entries don't consume an index.

**Positional and named entries may appear in any order.** Auto-indices are assigned sequentially to positional entries regardless of where named entries appear. For function calls, the binding priority chain (§Call Convention, C-PRIORITY) resolves positional arguments by index, then named arguments fill remaining parameters, then defaults apply.

## Heterogeneous Keys

**Allowed by default.** Integer and string keys can coexist in the same dict. Quoted strings are valid as keys, allowing keys with spaces or special characters: `["my key": value  "another:key": 42]`.

**Computed keys and the type checker:** Dict keys can be variable references (`[$k: value]`). The evaluator resolves computed keys at runtime. The type checker resolves them at compile time via literal types: if `$k` has type `StringLiteral("name")`, the field name is `"name"`. If the type is not a literal (e.g., plain `String`), the field is excluded from the Record type. See "Literal types enable computed key resolution" in the Type System section.

## Insertion Order

**Dicts preserve insertion order for iteration and display.** Semantically, entry order doesn't matter (letrec scoping). But iteration via `$keys`, `$values`, `$map` etc. follows the order entries appear in source. `$merge` preserves left order, appends new keys from right.

## Duplicate Keys Are Errors

**Duplicate keys in dict literals are an error.** Use `$merge` for intentional overrides.

```tinct
[name: Alice  name: Bob]              # → Error: duplicate key "name"
[call $merge [name: Alice] [name: Bob]]  # → [name: Bob]  (right-biased, intentional)
```

**Why:** Duplicate keys + lazy evaluation creates confusing semantics — depending on the scoping model, derived values may see different bindings of the same key. Prohibiting duplicates eliminates the ambiguity entirely and catches copy-paste errors.

## Numeric Types — `Int`, `Float`, `Number`

**Two concrete types: `Int(i64)` and `Float(f64)`.** `Number` is the supertype that accepts either. Integer literals carry their value: `42` has type `IntLiteral(42)`, which is a subtype of `Int`. Float literals do not have a literal type variant because floats cannot be dict keys.

```tinct
port: 8080                      # Int — no decimal point
pi: 3.14                        # Float — has decimal point
x@Int                           # must be an integer
y@Float                         # must be a float
z@Number                        # accepts either
```

**Arithmetic auto-promotes.** The compiler handles promotion with a fixed table — no typeclasses needed:

| Left | Op | Right | Result |
|------|-----|-------|--------|
| Int | `$+`, `$-`, `$*` | Int | Int |
| Int | any | Float | Float |
| Float | any | Int | Float |
| Float | any | Float | Float |
| any | `$/` | any | Float (always) |
| Int | `$quot`, `$mod` | Int | Int |

```tinct
[call $+ 5 3]                   # → 8 (Int)
[call $+ 5 3.0]                 # → 8.0 (Float)
[call $/ 10 3]                  # → 3.333... (Float — $/ always returns Float)
[call $quot 10 3]               # → 3 (Int — truncated integer division, prelude function using $trunc)
[call $mod 10 3]                # → 1 (Int — remainder)
```

**Integer arithmetic uses checked semantics.** `Int` operations (`$+`, `$-`, `$*`) use Rust's `checked_add`/`checked_sub`/`checked_mul`, so overflow returns an error rather than wrapping or panicking. This prevents silent data corruption on large values. Width-specific types like `Int32` could enforce narrower range constraints via the contracts system.

**Dict key integration:** `Int` values are directly usable as dict keys. `Float` values cannot be used as keys — floating-point equality semantics make them unreliable as hash keys.

**Width-specific types** (`Int32`, `Int64`, `Int128`, `Decimal`, etc.) are range constraints expressed through the contracts system, not new runtime representations. `Decimal` (if ever needed) would require a new Value variant.

The promotion table is built into the evaluator. User-defined numeric types participating in arithmetic would require type classes — see `doc/whatif/typeclasses.md` for the accepted design.

## No Null — Missing Keys Are Errors

**No `null` value in the language.** Accessing a nonexistent key is an error.

```tinct
[call $get $person name]         # → Alice
[call $get $person occupation]   # → Error: key "occupation" not found

# Safe alternative with default
[call $get-or $config timeout 30]  # → 30 if "timeout" is missing

# Check existence
[call $has? $config timeout]       # → true/false
```

**Why no null:**
- **Row polymorphism catches it at compile time.** A function taking `[name: String ...]` guarantees `name` exists. Most missing-key bugs never reach runtime.
- **Lazy eval provides a safety net.** `[x: [call $get $dict maybe-missing]]` doesn't error until `$x` is materialized. If you never use `$x`, no error.
- **No null confusion.** Can't confuse "key exists with null" vs "key is missing." Every key that exists has a real value.
- **Clean data representation.** Config files have no `null` noise — every key is meaningful.

**JSON null mapping:** Since Tinct has no null value, `$from-json` (and CLI stdin JSON injection) maps JSON `null` to `[]` (empty dict). This means it is impossible to distinguish "was null" from "was empty object" after conversion. This is an intentional trade-off -- Tinct's "no null" design prioritizes simplicity over round-trip fidelity with JSON.

## Data Access — Two Modes

Data access has two distinct modes: **key-based** (look up by key) and **position-based** (look up by insertion-order index). For dense lists `[a b c]` = `[0: a 1: b 2: c]`, these coincide. They diverge for sparse or mutated dicts.

**Key-based access** — brackets and dot notation:

```tinct
# Dot notation (string keys)
$person.name                    # ≡ [call $get $person name]
$config.database.host           # ≡ chained $get

# Bracket notation (any key type)
$data[5]                        # Integer key 5
$data[-1]                       # Integer key -1 (NOT last element)
$data[$key]                     # Computed key lookup
$config.services[0].host        # Mixed chaining — key 0 on services
```

**Rules:** Only `$ref.key` / `$ref[key]` — the left side must start with `$`. Bare `foo.bar` is just a string containing a dot. Brackets are always key-based — `$data[5]` finds the entry whose key is 5, not the 5th entry by position.

**Key-range slicing** with `..`:

```tinct
$data[2..5]                     # Entries with keys in [2, 5)
$data[2..]                      # Entries with keys ≥ 2
$data[..3]                      # Entries with keys < 3
```

Key-range slicing requires keys to be comparable. All-integer or all-string keys work; mixed-type keys are an error (caught by the type system). The range operator uses `..` (not `:`, which would conflict with the key-value separator).

**Position-based access** — stdlib functions:

```tinct
[call $nth $data 0]       # First entry (position 0)
[call $nth $data -1]      # Last entry (negative = from end)
[call $last $data]              # Last entry (alias)
[call $slice $data 2 5]         # Entries at positions 2, 3, 4
```

**Why the split:** Position-based access on a dict that has been mutated over time has less-than-useful ordering. Making it a function call (not syntax) signals that it's the unusual operation. For the common case of dense lists, `$data[0]` (key 0) and `[call $nth $data 0]` (position 0) return the same thing — you never need `$nth` unless you specifically want insertion-order semantics on sparse data.

### List vs Dict Operations — Renumbering Rule

**List operations require integer keys and always produce dense `[0..n]`.** Error on string keys. Dict operations preserve keys. Universal operations work on both and preserve keys.

```tinct
# List operations — integer keys only, always renumber
[call $first [alice bob carol]]         # → alice
[call $rest [alice bob carol]]          # → [bob carol] = [0: bob  1: carol]
[call $cons z [a b c]]                  # → [z a b c] = [0: z  1: a  2: b  3: c]
[call $conj [a b c] d]                  # → [a b c d] = [0: a  1: b  2: c  3: d]
[call $concat [a b] [c d]]             # → [a b c d] = [0: a  1: b  2: c  3: d]
[call $reverse [a b c]]                 # → [c b a] = [0: c  1: b  2: a]
[call $sort [cherry apple banana]]      # → [apple banana cherry] — sorts by value, discards original keys
[call $reindex [0: a  5: b  10: c]]     # → [a b c] = [0: a  1: b  2: c]
```

**Why this split:**
- No ambiguity about which operations renumber — it's determined by the category, not the data
- List operations always give you clean, predictable lists
- Dict operations never silently destroy your key structure
- `$filter` returns a Seq of matching values (since inclusion requires predicate evaluation, keys are not preserved) — use `$collect` to get a dict back
- The type system enforces the boundary: list operations require `[a]` (integer-keyed)

```tinct
# $filter returns a Seq of matching values (dual-dispatch)
$data: [alice bob carol dave]
[call $filter [fn [x] [call $not [call $= $x bob]]] $data]
# → Seq(alice, carol, dave)    use $collect for a dict

# Pipe through $collect for a clean list
[call $collect [call $filter [fn [x] [call $not [call $= $x bob]]] $data]]
# → [0: alice  1: carol  2: dave]

# $filter on string-keyed dicts also returns Seq of values
[call $collect [call $filter [fn [v] [call $> $v 0]] [x: 1  y: -2  z: 3]]]
# → [0: 1  1: 3]
```

**`$conj` on sparse data:** `$conj` delegates to `$append`, which uses the maximum existing integer key + 1 as the new key (or 0 if no integer keys exist). This avoids key collisions even on sparse data:

```tinct
# Dense list — $conj works as expected
[call $conj [a b c] d]                  # → [0: a  1: b  2: c  3: d]

# Sparse data — no collision, key 11 is used (max 10 + 1)
$sparse: [0: a  5: b  10: c]
[call $conj $sparse d]                  # → [0: a  5: b  10: c  11: d]
```

### Access Chain Evaluation — Formal Specification

Formalizes the three access forms (dot, bracket, range) as an access algebra with compositional chain semantics. Access chains are the primary data extraction mechanism in tinct — they desugar to nested AST nodes that the evaluator reduces inside-out, forcing the target at each step.

#### Part 1: Access Algebra

An **access chain** is a sequence of projections applied left-to-right to a target expression. The parser produces nested AST nodes; the algebra makes the compositional structure explicit.

**Projections.** A projection `π` extracts data from a dict:

```
π ::= dot(f)              — field access by literal string key f
    | bracket(e)          — field access by evaluated expression e
    | range(s?, e?)       — key-range slice with optional bounds [s, e)
```

**Chains.** An access chain `C = π₁ · π₂ · ... · πₙ` applied to target expression `t` evaluates as left-to-right composition:

```
eval_chain(t, [], ρ, d) = eval(t, ρ, d)                          (empty chain)
eval_chain(t, [π₁, ...πₙ], ρ, d) = eval_chain(apply(π₁, t, ρ, d), [π₂, ...πₙ], ρ, d)
```

**Parser correspondence:** The parser produces nested AST nodes for chains. `$a.b[0].c` parses as:

```
DotAccess(
  BracketAccess(
    DotAccess(VarRef("a"), "b"),
    Int(0)),
  "c")
```

The evaluator reduces inside-out: first `eval(VarRef("a"))`, then `apply(dot("b"), ...)`, then `apply(bracket(0), ...)`, then `apply(dot("c"), ...)`. This inside-out reduction is equivalent to the left-to-right chain evaluation defined above.

#### Part 2: Projection Rules

Each projection forces its target to a `Dict`, then extracts by key. All three rules share a common forcing step formalized as `force_dict`.

**[FORCE-DICT]** — Common target forcing

```
θ_target = eval(target, ρ, d+1)
v = force(θ_target, d+1)                    (inherent materialization — must know dict structure)
v = Dict(map)                               (target must be Dict; type error otherwise)
────────────────────────────────────────────
force_dict(target, ρ, d) ⇒ map
```

If `v` is not a `Dict`, evaluation fails with `type_mismatch("Dict", v.type_name(), span)`. This is inherent materialization (§Selective Materialization) — the dict structure must be known to perform key lookup. FORCE-DICT is a composite rule combining `eval`, `force`, and pattern match — it is not a primitive judgment of the Thunk Lifecycle. All three projection rules below conclude with `⇒ Rc<Thunk>` — ACCESS-DOT and ACCESS-BRACKET return an alias to an existing thunk in the dict, while ACCESS-RANGE wraps its result in a fresh `Materialized` thunk.

**[ACCESS-DOT]** — Dot access: `$target.field`

```
map = force_dict(target, ρ, d)
key = String(field)                          (field is a literal string from the AST)
map[key] = θ                                 (look up key; error if absent)
────────────────────────────────────────────
eval_dot(target, field, ρ, d) ⇒ θ
```

Error case: if `key ∉ dom(map)`, error `key_not_found(field, span)`. No default — missing keys are always errors (§No Null — Missing Keys Are Errors).

**[ACCESS-BRACKET]** — Bracket access: `$target[key_expr]`

```
map = force_dict(target, ρ, d)
key = eval_key(key_expr, ρ, d)              (evaluate key expression to String or Int)
map[key] = θ                                 (look up key; error if absent)
────────────────────────────────────────────
eval_bracket(target, key_expr, ρ, d) ⇒ θ
```

`eval_key` evaluates the key expression and materializes it to obtain a concrete `String` or `Int` key. This is the same `eval_key` used by DICT-SCOPE (§Scope Chain Semantics) — key evaluation is shared infrastructure.

Error case: if `key ∉ dom(map)`, error `key_not_found(key, span)`.

**[ACCESS-RANGE]** — Range access: `$target[start..end]`

```
map = force_dict(target, ρ, d)
s = start.map(|e| eval_key(e, ρ, d))        (optional start bound, evaluated)
e = end.map(|e| eval_key(e, ρ, d))          (optional end bound, evaluated)

result = {}
∀(k, θ) ∈ map (in insertion order):
  key_in_range(k, s, e) ⟹ result[k] ← θ   (include matching entries)
────────────────────────────────────────────
eval_range(target, start, end, ρ, d) ⇒ Materialized(Dict(result))
```

**Range semantics:** Half-open interval `[start, end)` — start inclusive, end exclusive. When `start` is `None`, all keys from the beginning are included. When `end` is `None`, all keys to the end are included. When both are `None` (`$data[..]`), all entries are included (identity slice).

**`key_in_range` comparability:**

```
key_in_range(k, s, e):
  ∀bound ∈ {s, e} where bound ≠ None:
    k.partial_cmp(bound) must be Some(_)     (keys must be comparable)
  after_start = s = None ∨ k ≥ s
  before_end  = e = None ∨ k < e
  return after_start ∧ before_end
```

`Key::PartialOrd` returns `Some` for same-type comparisons (`Int-Int`, `String-String`) and `None` for mixed types (`Int-String`). When `partial_cmp` returns `None`, evaluation fails with `"range access requires comparable key types"`. Both bounds are checked unconditionally — a key that fails one bound may still error on the other if types are incomparable. In practice, this is unreachable because the type system requires homogeneous key types for range-accessed dicts (§Type Inference Algorithm).

**Result construction:** ACCESS-RANGE returns a `Materialized(Dict(result))` — unlike ACCESS-DOT and ACCESS-BRACKET which return an existing thunk from the dict, ACCESS-RANGE constructs a new dict. The individual entry thunks `θ` are shared (`Rc::clone`) with the source dict, preserving memoization. The `key_in_range` predicate determines the result *set* independently of iteration order (it tests each key against the bounds). Insertion order from the source dict is preserved in the result dict, affecting only the ordering of entries, not which entries are included.

#### Part 3: Error Taxonomy

Four error classes, each mapping to a specific point in the projection rules:

| Error | Rule | Condition | Message |
|-------|------|-----------|---------|
| Target not a Dict | FORCE-DICT | `v` is not `Dict` | `type_mismatch("Dict", v.type_name())` |
| Key not found (dot) | ACCESS-DOT | `String(field) ∉ dom(map)` | `key_not_found(field)` |
| Key not found (bracket) | ACCESS-BRACKET | `key ∉ dom(map)` | `key_not_found(key)` |
| Incomparable keys (range) | ACCESS-RANGE | `partial_cmp` returns `None` | `"range access requires comparable key types"` |

Error context is enriched via `push_frame`: dot access adds `"accessing .{field}"`, bracket adds `"accessing [..]"`, range adds `"accessing [..:..]"`. This stack frame identifies which step in a chain failed.

#### Part 4: Chain Properties

Five properties that hold for all access chains.

**Property 1: Step-wise Forcing**

*Statement:* Each projection in a chain invokes FORCE-DICT exactly once. In a chain `π₁ · π₂ · ... · πₙ`, FORCE-DICT is invoked `n` times — once per step. FORCE-DICT evaluates and forces the target — if the target thunk is already `Materialized`, forcing is a cache hit (FORCE-CACHED from §Thunk Lifecycle).

*Proof sketch:* By induction on chain length. Each `apply(πᵢ, ...)` invokes FORCE-DICT, which calls `force(θ, d+1)`. The result of step `i` becomes the target of step `i+1`. No step forces the target of a different step. ∎

**Property 2: Result Laziness**

*Statement:* ACCESS-DOT and ACCESS-BRACKET return the thunk stored in the dict without forcing it. The result may be `Unevaluated`, `PendingBuiltin`, `PendingCall`, or `Materialized` — access does not trigger evaluation of the accessed value.

*Proof sketch:* Both rules return `Rc::clone(thunk)` from `map.get(&key)` — a pointer copy, not a `force` call. The thunk's state is unchanged by the access. ACCESS-RANGE also preserves laziness of individual entries (shared via `Rc::clone`), though it constructs a new `Materialized(Dict(...))` wrapper. ∎

**Property 3: Error Short-Circuiting**

*Statement:* If projection `πᵢ` in a chain fails, projections `πᵢ₊₁, ..., πₙ` are never evaluated.

*Proof sketch:* By the chain recurrence, `eval_chain(t, [π₁, ...πₙ], ρ, d)` first computes `apply(π₁, t, ρ, d)`. If this returns an error, the recurrence has no value to pass to the next step, so the chain terminates with that error. By induction, no subsequent projection is evaluated. ∎

**Property 4: Depth Consumption**

*Statement:* A chain of length `n` consumes `n` depth levels — each FORCE-DICT invocation increments depth by 1 (via `eval(target, ρ, d+1)` and `materialize(θ, d+1)` in each access function).

*Proof sketch:* By inspection of FORCE-DICT, which passes `d+1` to both `eval` and `materialize`. Each chain step invokes FORCE-DICT once (Property 1), so `n` steps consume `n` depth levels. For `MAX_EVAL_DEPTH = 256` and typical chain lengths (1–5), this is negligible. The CEK machine removes MAX_EVAL_DEPTH, making this property moot. ∎

**Property 5: Sharing Preservation**

*Statement:* ACCESS-DOT and ACCESS-BRACKET return an `Rc::clone` of the thunk stored in the dict — an alias, not a copy. If the same field is accessed twice, both accesses obtain pointers to the same `Rc<Thunk>`. Once the first access forces it, the second access gets FORCE-CACHED (§Thunk Lifecycle). ACCESS-RANGE creates a new dict wrapper but shares entry thunks via `Rc::clone`, so memoization is preserved for individual entries.

*Proof sketch:* ACCESS-DOT and ACCESS-BRACKET return `Rc::clone(thunk)` from `map.get(&key)`. The `Rc` reference count increases, but both the dict entry and the accessor hold pointers to the same `Thunk`. When either forces it, the thunk transitions to `Materialized` (or `Failed`), and subsequent accesses via any alias see the cached state. This is the Launchbury (1993) sharing guarantee applied to record projection — access is observation, not duplication. ∎

#### Part 5: Type System Correspondence

Access chain type checking generates row constraints via Remy-style row-variable unification (see §Row-Variable Unification in [Type System Extensions](07-type-extensions.md) Part 5). The target type is inferred first, then field access generates constraints of the form `unify(typeof(x), Record([field: α], ρ))`, binding `α` and `ρ` via row unification — enabling the type checker to infer field requirements from usage without annotations.

The type checker mirrors the access algebra with type-level projections:

| Runtime rule | Type rule | Type-level behavior |
|-------------|-----------|-------------------|
| ACCESS-DOT | `check_dot_access` | `Record(fields) → fields[f]`; open record → `Any`; closed + missing → error |
| ACCESS-BRACKET | `check_bracket_access` | Literal key → exact field lookup; variable key → `Any`; open record → `Any` |
| ACCESS-RANGE | `check_range_access` | Bounds must be `Int` or `Str`; result type = target type (preserves record type) |

**Type variable access:** Accessing a field on a type variable (`TypeVar(α)`) is a type error (`typecheck.rs:313` falls through to `not_a_record`). Constraint-based row unification would bind `α` to `Record([field: β], ρ)` — see §Row-Variable Unification in [Type System Extensions](07-type-extensions.md). Row variables (`RowVar(r)`) appearing in record types are treated as markers for openness during access type checking; they are not bound to remainder types during access operations (consistent with U-REC in §Type Inference Algorithm).

**Open records and Any:** When a dot or bracket access targets an open record (`Record(fields, Open)` or `Record(fields, RowVar(_))`) and the field is not in `fields`, the type checker returns `Any` rather than an error. This reflects Tinct's gradual typing design: open records may contain fields not visible to the type checker. Rather than reject valid programs, the type checker admits the access but types the result as `Any`, deferring validation to runtime. This is sound because `Any` serves as both top and bottom type (S-ANY-TOP, S-ANY-BOT in §Type Inference Algorithm) — values of any type flow through `Any` positions. For closed records, a missing field is a static error.

**Bracket key precision:** When the bracket key is a literal (`Expr::Str` or `Expr::Int`) or has a singleton type (`StringLiteral(s)` or `IntLiteral(n)`), the type checker performs exact field lookup. When the key is a variable with type `Str`, `Int`, or `Any`, the result type is `Any` — since the key value is not known until runtime, the type checker cannot determine which field will be accessed, so it conservatively returns `Any`. This is the trade-off between expressiveness (allow computed keys) and precision (lose static type information).

**Range type preservation:** Range access conservatively types the result as the target type rather than attempting to narrow the field set (`typecheck.rs:384` returns `target_ty` unchanged). This is sound: the result dict is structurally a subtype of the target type (it contains a subset of the fields). Precise inference would require dependent types or refinement types to track which fields are included based on the runtime range bounds. The type checker does not currently verify that range bounds have compatible types with each other or with the target record's key types — `$data["a"..3]` with a String start and Int end passes type checking but fails at runtime. This is a known completeness gap; statically rejecting mixed-type bounds would require unifying the bound types.

#### Part 6: Implementation Correspondence

| Formal rule | Implementation | Source |
|------------|----------------|--------|
| FORCE-DICT | Inlined in each access function (`eval` + `materialize` on target) | `eval.rs:1022-1143` |
| ACCESS-DOT | `eval_dot_access()` | `eval.rs:1022-1056` |
| ACCESS-BRACKET | `eval_bracket_access()` | `eval.rs:1059-1094` |
| ACCESS-RANGE | `eval_range_access()` | `eval.rs:1099-1143` |
| `key_in_range` | `key_in_range()` | `eval.rs:26-46` |
| `Key::PartialOrd` | `impl PartialOrd for Key` | `value.rs:34-42` |
| Chain nesting | Parser produces nested `DotAccess`/`BracketAccess`/`RangeAccess` AST nodes | `ast.rs:79-93` |
| Type-level dot | `check_dot_access()` | `typecheck.rs:297-315` |
| Type-level bracket | `check_bracket_access()` | `typecheck.rs:317-357` |
| Type-level range | `check_range_access()` | `typecheck.rs:359-387` |

#### Part 7: Worked Examples

**Example 1: Chained dot access**

```tinct
[config: [database: [host: localhost  port: 5432]]]

[call $str $config.database.host]
```

Chain: `dot("database") · dot("host")` applied to `$config`.
1. `eval(VarRef("config"), ρ)` → `θ_config`
2. `force_dict(θ_config)` → `{database: θ_db}`. `map[String("database")]` → `θ_db`. Result: `θ_db` (lazy).
3. `force_dict(θ_db)` → `{host: θ_host, port: θ_port}`. `map[String("host")]` → `θ_host`. Result: `θ_host` (lazy).
4. `$str` forces `θ_host` → `"localhost"`.

Note: `θ_port` is never forced — Property 2 (result laziness) means accessing `.host` does not evaluate `.port`.

**Example 2: Mixed chain with bracket**

```tinct
$services[0].host
```

Chain: `bracket(Int(0)) · dot("host")`.
1. `force_dict($services)` → map. `eval_key(Int(0))` → `Key::Int(0)`. `map[Int(0)]` → `θ_svc0`.
2. `force_dict(θ_svc0)` → `{host: θ_host, ...}`. `map[String("host")]` → `θ_host`.

**Example 3: Range access**

```tinct
$data: [a: 1  b: 2  c: 3  d: 4]
$data[b..d]
```

`force_dict($data)` → `{a: θ₁, b: θ₂, c: θ₃, d: θ₄}`. Bounds: `s = String("b")`, `e = String("d")`.
- `key_in_range(String("a"), "b", "d")`: `"a" < "b"` → `after_start` = false → exclude.
- `key_in_range(String("b"), "b", "d")`: `"b" ≥ "b"` ∧ `"b" < "d"` → include.
- `key_in_range(String("c"), "b", "d")`: `"c" ≥ "b"` ∧ `"c" < "d"` → include.
- `key_in_range(String("d"), "b", "d")`: `"d" ≥ "b"` ∧ `"d" < "d"` → `before_end` = false → exclude.
- Result: `Materialized(Dict({b: θ₂, c: θ₃}))`. Half-open: start inclusive, end exclusive.
