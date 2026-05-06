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
[name: "Alice"  age: 30]        # All keyed — a "dict"
[a b c]                         # All auto-indexed — a "list" = [0: a  1: b  2: c]
[f x timeout: 60]               # Mixed — positional + named (implied call)
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

**Duplicate keys in dict literals are an error.** Use `merge` for intentional overrides.

```tinct
[name: "Alice"  name: "Bob"]              # → Error: duplicate key "name"
[merge [name: "Alice"] [name: "Bob"]]     # → [name: "Bob"]  (right-biased, intentional)
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
| Int | `+`, `-`, `*` | Int | Int |
| Int | any | Float | Float |
| Float | any | Int | Float |
| Float | any | Float | Float |
| any | `/` | any | Float (always) |
| Int | `quot`, `mod` | Int | Int |

```tinct
[+ 5 3]                         # → 8 (Int)
[+ 5 3.0]                       # → 8.0 (Float)
[/ 10 3]                        # → 3.333... (Float — / always returns Float)
[quot 10 3]                     # → 3 (Int — truncated integer division, prelude function using trunc)
[mod 10 3]                      # → 1 (Int — remainder)
```

**Integer arithmetic uses checked semantics.** `Int` operations (`+`, `-`, `*`) use Rust's `checked_add`/`checked_sub`/`checked_mul`, so overflow returns an error rather than wrapping or panicking. This prevents silent data corruption on large values. Width-specific types like `Int32` could enforce narrower range constraints via the contracts system.

**Dict key integration:** `Int` values are directly usable as dict keys. `Float` values cannot be used as keys — floating-point equality semantics make them unreliable as hash keys.

**Width-specific types** (`Int32`, `Int64`, `Int128`, `Decimal`, etc.) are range constraints expressed through the contracts system, not new runtime representations. `Decimal` (if ever needed) would require a new Value variant.

The promotion table is built into the evaluator. User-defined numeric types participating in arithmetic would require type classes — see `doc/whatif/typeclasses.md` for the accepted design.

## No Null — Missing Keys Are Errors

**No `null` value in the language.** Accessing a nonexistent key is an error.

```tinct
[get person "name"]              # → "Alice"
[get person "occupation"]        # → Error: key "occupation" not found

# Safe alternative with default
[get-or config "timeout" 30]    # → 30 if "timeout" is missing

# Check existence
[has? config "timeout"]          # → true/false
```

**Why no null:**
- **Row polymorphism catches it at compile time.** A function taking `[name: String ...]` guarantees `name` exists. Most missing-key bugs never reach runtime.
- **Lazy eval provides a safety net.** `[x: [get dict "maybe-missing"]]` doesn't error until `x` is materialized. If you never use `x`, no error.
- **No null confusion.** Can't confuse "key exists with null" vs "key is missing." Every key that exists has a real value.
- **Clean data representation.** Config files have no `null` noise — every key is meaningful.

**JSON null mapping:** Since Tinct has no null value, `from-json` (and CLI stdin JSON injection) maps JSON `null` to `[]` (empty dict). This means it is impossible to distinguish "was null" from "was empty object" after conversion. This is an intentional trade-off -- Tinct's "no null" design prioritizes simplicity over round-trip fidelity with JSON.

## Data Access — Two Modes

Data access has two distinct modes: **key-based** (look up by key) and **position-based** (look up by insertion-order index). For dense lists `[a b c]` = `[0: a 1: b 2: c]`, these coincide. They diverge for sparse or mutated dicts.

**Key-based access** — dot notation and `get` builtin:

```tinct
# Dot notation (string keys and integer dot access)
person.name                     # string key "name"
config.database.host            # chained string key access
data.0                          # integer dot access — looks up Key::Int(0)

# get builtin (dynamic key access, replaces bracket access)
[get 5 data]                    # Integer key 5
[get "name" data]               # String key "name"
[get $key data]                 # Computed key lookup
[get 0 config.services].host    # Dynamic key then dot chain
```

**Rules:** Identifiers can start access chains directly — `foo.bar` and `$foo.bar` are both valid. `[get key data]` finds the entry whose key matches `key`, not the nth entry by position.

**Note:** Bracket access (`data[5]`, `data[$key]`) was removed in access-pipeline-phase2. Use `[get key data]` for integer and dynamic key access.

**Subsequence operations** — stdlib functions:

```tinct
[slice data 2 5]                # Entries at positions 2, 3, 4 (position-based)
[take 3 data]                   # First 3 entries
[drop 2 data]                   # All entries after the first 2
```

**Note:** Range access (`data[2..5]`, `data[2..]`, `data[..3]`) was removed in access-pipeline-phase2. Use `slice`, `take`, and `drop` for subsequences.

**Position-based access** — stdlib functions:

```tinct
[nth data 0]                    # First entry (position 0)
[nth data -1]                   # Last entry (negative = from end)
[last data]                     # Last entry (alias)
[slice data 2 5]                # Entries at positions 2, 3, 4
```

**Why the split:** Position-based access on a dict that has been mutated over time has less-than-useful ordering. Making it a function call (not syntax) signals that it's the unusual operation. For the common case of dense lists, `[get 0 data]` (key 0) and `[nth data 0]` (position 0) return the same thing — you never need `nth` unless you specifically want insertion-order semantics on sparse data.

### Lazy Sequences — Value::Seq

**Lazy sequences (`Value::Seq`) are a runtime-only value type** representing infinite or demand-driven data (from `$range`, `$repeat`, `$cycle`, `$iterate`, etc.). They exist alongside `Dict`, `Int`, `Float`, `String`, `Bool`, and `Function` in the value representation. Sequences have no literal syntax — they are produced by builtin functions and consumed by sequence operations like `$map`, `$filter`, `$take`, `$collect`.

Sequences are dual-dispatch targets: `$map` on a Seq returns a lazy Seq, `$filter` on a Seq returns a lazy Seq. Use `$collect` to materialize a Seq to a dense dict. Attempting operations that require full materialization (like `$sort` or `$length`) on an infinite Seq will error. See doc/08-evaluation.md §Lazy Sequences for implementation details and laziness semantics.

### List vs Dict Operations — Renumbering Rule

**List operations require integer keys and always produce dense `[0..n]`.** Error on string keys. Dict operations preserve keys. Universal operations work on both and preserve keys.

```tinct
# List operations — integer keys only, always renumber
[first [alice bob carol]]               # → alice
[rest [alice bob carol]]                # → [bob carol] = [0: bob  1: carol]
[cons z [a b c]]                        # → [z a b c] = [0: z  1: a  2: b  3: c]
[conj [a b c] d]                        # → [a b c d] = [0: a  1: b  2: c  3: d]
[concat [a b] [c d]]                    # → [a b c d] = [0: a  1: b  2: c  3: d]
[reverse [a b c]]                       # → [c b a] = [0: c  1: b  2: a]
[sort [cherry apple banana]]            # → [apple banana cherry] — sorts by value, discards original keys
[reindex [0: a  5: b  10: c]]           # → [a b c] = [0: a  1: b  2: c]
```

**Why this split:**
- No ambiguity about which operations renumber — it's determined by the category, not the data
- List operations always give you clean, predictable lists
- Dict operations never silently destroy your key structure
- `filter` returns a Seq of matching values (since inclusion requires predicate evaluation, keys are not preserved) — use `collect` to get a dict back
- The type system enforces the boundary: list operations require `[a]` (integer-keyed)

```tinct
# filter returns a Seq of matching values (dual-dispatch)
data: [alice bob carol dave]
[filter [fn [x] [not [= x bob]]] data]
# → Seq(alice, carol, dave)    use collect for a dict

# Pipe through collect for a clean list
[collect [filter [fn [x] [not [= x bob]]] data]]
# → [0: alice  1: carol  2: dave]

# filter on string-keyed dicts also returns Seq of values
[collect [filter [fn [v] [> v 0]] [x: 1  y: -2  z: 3]]]
# → [0: 1  1: 3]
```

**`conj` on sparse data:** `conj` delegates to `append`, which uses the maximum existing integer key + 1 as the new key (or 0 if no integer keys exist). This avoids key collisions even on sparse data:

```tinct
# Dense list — conj works as expected
[conj [a b c] d]                        # → [0: a  1: b  2: c  3: d]

# Sparse data — no collision, key 11 is used (max 10 + 1)
sparse: [0: a  5: b  10: c]
[conj sparse d]                         # → [0: a  5: b  10: c  11: d]
```

### Access Chain Evaluation — Formal Specification

Formalizes access forms (dot and `get` builtin) as an access algebra with compositional chain semantics. Access chains are the primary data extraction mechanism in tinct — they desugar to nested AST nodes that the evaluator reduces inside-out, forcing the target at each step.

**Note:** Bracket access (`data[key]`) and range access (`data[2..5]`) were removed in access-pipeline-phase2. The formal specification below covers the current implementation: dot access and the `get` builtin. The ACCESS-BRACKET and ACCESS-RANGE rules below are retained as historical reference (they document the removed evaluation rules).

#### Part 1: Access Algebra

An **access chain** is a sequence of projections applied left-to-right to a target expression. The parser produces nested AST nodes; the algebra makes the compositional structure explicit.

**Projections.** A projection `π` extracts data from a dict:

```
π ::= dot(f)              — field access by literal string key f (or integer key n for dot-int access)
```

(Historical: `bracket(e)` and `range(s?, e?)` projections were removed in access-pipeline-phase2. Use `[get key data]` for dynamic key access and `[slice data start end]` for subsequences.)

**Chains.** An access chain `C = π₁ · π₂ · ... · πₙ` applied to target expression `t` evaluates as left-to-right composition:

```
eval_chain(t, [], ρ, d) = eval(t, ρ, d)                          (empty chain)
eval_chain(t, [π₁, ...πₙ], ρ, d) = eval_chain(apply(π₁, t, ρ, d), [π₂, ...πₙ], ρ, d)
```

**Parser correspondence:** The parser produces nested AST nodes for chains. `$a.b.0.c` parses as:

```
DotAccess(
  DotAccess(
    DotAccess(VarRef("a"), "b"),
    Int(0)),
  "c")
```

(Bracket access was removed in access-pipeline-phase2. Use `[get 0 $a.b].c` to look up integer key 0 then dot-access "c".)

The evaluator reduces inside-out: first `eval(VarRef("a"))`, then `apply(dot("b"), ...)`, then `apply(dot(0), ...)`, then `apply(dot("c"), ...)`. This inside-out reduction is equivalent to the left-to-right chain evaluation defined above.

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

If `v` is not a `Dict`, evaluation fails with `type_mismatch("Dict", v.type_name(), span)`. This is inherent materialization (§Selective Materialization) — the dict structure must be known to perform key lookup. FORCE-DICT is a composite rule combining `eval`, `force`, and pattern match — it is not a primitive judgment of the Thunk Lifecycle. ACCESS-DOT returns an alias to an existing thunk in the dict.

**[ACCESS-DOT]** — Dot access: `$target.field`

```
map = force_dict(target, ρ, d)
key = String(field)                          (field is a literal string from the AST)
map[key] = θ                                 (look up key; error if absent)
────────────────────────────────────────────
eval_dot(target, field, ρ, d) ⇒ θ
```

Error case: if `key ∉ dom(map)`, error `key_not_found(field, span)`. No default — missing keys are always errors (§No Null — Missing Keys Are Errors).

**[ACCESS-BRACKET]** — Bracket access (historical — removed in access-pipeline-phase2)

Bracket access (`$target[key_expr]`) was removed. Use `[get key_expr target]` (the `get` builtin) for dynamic key access. The `get` builtin evaluates its key argument and materializes it to a concrete `String` or `Int` key, then performs the lookup. Error if key not found.

**[ACCESS-RANGE]** — Range access (historical — removed in access-pipeline-phase2)

Range access (`$target[start..end]`) was removed. Use `[slice target start end]`, `[take n target]`, or `[drop n target]` for subsequences. These builtins work on position (insertion order), not on key values.

#### Part 3: Error Taxonomy

Error classes for current access forms:

| Error | Rule | Condition | Message |
|-------|------|-----------|---------|
| Target not a Dict | FORCE-DICT | `v` is not `Dict` | `type_mismatch("Dict", v.type_name())` |
| Key not found (dot) | ACCESS-DOT | `String(field) ∉ dom(map)` | `key_not_found(field)` |
| Key not found (`get`) | `get` builtin | `key ∉ dom(map)` | `key_not_found(key)` |

Error context is enriched via `push_frame`: dot access adds `"accessing .{field}"`. (Bracket and range push_frame entries were removed with ACCESS-BRACKET and ACCESS-RANGE in access-pipeline-phase2.)

#### Part 4: Chain Properties

Five properties that hold for all access chains.

**Property 1: Step-wise Forcing**

*Statement:* Each projection in a chain invokes FORCE-DICT exactly once. In a chain `π₁ · π₂ · ... · πₙ`, FORCE-DICT is invoked `n` times — once per step. FORCE-DICT evaluates and forces the target — if the target thunk is already `Materialized`, forcing is a cache hit (FORCE-CACHED from §Thunk Lifecycle).

*Proof sketch:* By induction on chain length. Each `apply(πᵢ, ...)` invokes FORCE-DICT, which calls `force(θ, d+1)`. The result of step `i` becomes the target of step `i+1`. No step forces the target of a different step. ∎

**Property 2: Result Laziness**

*Statement:* ACCESS-DOT returns the thunk stored in the dict without forcing it. The result may be `Unevaluated`, `PendingBuiltin`, `PendingCall`, or `Materialized` — access does not trigger evaluation of the accessed value.

*Proof sketch:* ACCESS-DOT returns `Rc::clone(thunk)` from `map.get(&key)` — a pointer copy, not a `force` call. The thunk's state is unchanged by the access. ∎

**Property 3: Error Short-Circuiting**

*Statement:* If projection `πᵢ` in a chain fails, projections `πᵢ₊₁, ..., πₙ` are never evaluated.

*Proof sketch:* By the chain recurrence, `eval_chain(t, [π₁, ...πₙ], ρ, d)` first computes `apply(π₁, t, ρ, d)`. If this returns an error, the recurrence has no value to pass to the next step, so the chain terminates with that error. By induction, no subsequent projection is evaluated. ∎

**Property 4: Depth Consumption**

*Statement:* A chain of length `n` consumes `n` depth levels — each FORCE-DICT invocation increments depth by 1 (via `eval(target, ρ, d+1)` and `materialize(θ, d+1)` in each access function).

*Proof sketch:* By inspection of FORCE-DICT, which passes `d+1` to both `eval` and `materialize`. Each chain step invokes FORCE-DICT once (Property 1), so `n` steps consume `n` depth levels. For `MAX_EVAL_DEPTH = 256` and typical chain lengths (1–5), this is negligible. The CEK machine removes MAX_EVAL_DEPTH, making this property moot. ∎

**Property 5: Sharing Preservation**

*Statement:* ACCESS-DOT returns an `Rc::clone` of the thunk stored in the dict — an alias, not a copy. If the same field is accessed twice, both accesses obtain pointers to the same `Rc<Thunk>`. Once the first access forces it, the second access gets FORCE-CACHED (§Thunk Lifecycle).

*Proof sketch:* ACCESS-DOT returns `Rc::clone(thunk)` from `map.get(&key)`. The `Rc` reference count increases, but both the dict entry and the accessor hold pointers to the same `Thunk`. When either forces it, the thunk transitions to `Materialized` (or `Failed`), and subsequent accesses via any alias see the cached state. This is the Launchbury (1993) sharing guarantee applied to record projection — access is observation, not duplication. ∎

#### Part 5: Type System Correspondence

Access chain type checking generates row constraints via Remy-style row-variable unification (see §Row-Variable Unification in [Type System Extensions](07-type-extensions.md) Part 5). The target type is inferred first, then field access generates constraints of the form `unify(typeof(x), Record([field: α], ρ))`, binding `α` and `ρ` via row unification — enabling the type checker to infer field requirements from usage without annotations.

The type checker mirrors the access algebra with type-level projections:

| Runtime rule | Type rule | Type-level behavior |
|-------------|-----------|-------------------|
| ACCESS-DOT | `check_dot_access` | `Record(fields) → fields[f]`; open record → `Any`; closed + missing → error |
| ACCESS-DOT (Int) | `check_dot_access_int` | Integer dot access `.N`; looks up `Key::Int(N)`; open record → `Any` |
| `get` builtin | `check_bracket_access` (historical) | Now handled as a regular builtin call; key access via `[get key data]` |

**Type variable access:** Accessing a field on a type variable (`TypeVar(α)`) is a type error (`typecheck.rs:313` falls through to `not_a_record`). Constraint-based row unification would bind `α` to `Record([field: β], ρ)` — see §Row-Variable Unification in [Type System Extensions](07-type-extensions.md). Row variables (`RowVar(r)`) appearing in record types are treated as markers for openness during access type checking; they are not bound to remainder types during access operations (consistent with U-REC in §Type Inference Algorithm).

**Open records and Any:** When a dot access targets an open record (`Record(fields, Open)` or `Record(fields, RowVar(_))`) and the field is not in `fields`, the type checker returns `Any` rather than an error. This reflects Tinct's gradual typing design: open records may contain fields not visible to the type checker. Rather than reject valid programs, the type checker admits the access but types the result as `Any`, deferring validation to runtime. This is sound because `Any` serves as both top and bottom type (S-ANY-TOP, S-ANY-BOT in §Type Inference Algorithm) — values of any type flow through `Any` positions. For closed records, a missing field is a static error.

**`get` builtin precision:** When the key passed to `[get key data]` is a literal (`Expr::Str` or `Expr::Int`), the type checker performs exact field lookup. When the key is a variable with type `Str`, `Int`, or `Any`, the result type is `Any` — since the key value is not known until runtime, the type checker cannot determine which field will be accessed. The `get` builtin is now checked as a regular call rather than via the historical `check_bracket_access` function (removed in access-pipeline-phase2).

#### Part 6: Implementation Correspondence

| Formal rule | Implementation | Source |
|------------|----------------|--------|
| FORCE-DICT | Inlined in each access function (`eval` + `materialize` on target) | `eval_materialize.rs` |
| ACCESS-DOT | `eval()` returns `Unevaluated` thunk; `force_step()` via `DotAccessForce` continuation | `eval_materialize.rs` |
| `Key::PartialOrd` | `impl PartialOrd for Key` | `value.rs` |
| Chain nesting | Parser produces nested `DotAccess` AST nodes | `ast.rs` |
| Type-level dot | `check_dot_access()` | `typecheck.rs` |
| Note: `BracketForceTarget`, `eval_range_access`, `key_in_range`, `check_bracket_access`, `check_range_access` | All removed in access-pipeline-phase2 | — |

#### Part 7: Worked Examples

**Example 1: Chained dot access**

```tinct
[config: [database: [host: "localhost"  port: 5432]]]

[str config.database.host]
```

Chain: `dot("database") · dot("host")` applied to `config`.
1. `eval(VarRef("config"), ρ)` → `θ_config`
2. `force_dict(θ_config)` → `{database: θ_db}`. `map[String("database")]` → `θ_db`. Result: `θ_db` (lazy).
3. `force_dict(θ_db)` → `{host: θ_host, port: θ_port}`. `map[String("host")]` → `θ_host`. Result: `θ_host` (lazy).
4. `str` forces `θ_host` → `"localhost"`.

Note: `θ_port` is never forced — Property 2 (result laziness) means accessing `.host` does not evaluate `.port`.

**Example 2: Dynamic key access with `get` builtin**

```tinct
[get 0 services].host
```

`[get 0 services]` calls the `get` builtin with key `Int(0)` and dict `services`. The builtin materializes `services`, looks up `Key::Int(0)` → `θ_svc0`. Then `.host` dot-accesses `θ_svc0`.

(Historical: The old `services[0].host` — bracket access followed by dot — was removed in access-pipeline-phase2.)

**Example 3: Subsequence with `slice` (replaces range access)**

```tinct
data: [a: 1  b: 2  c: 3  d: 4]
[slice data 1 3]
```

`[slice data 1 3]` returns entries at positions 1 and 2 (half-open interval `[1, 3)` by insertion order), yielding `[0: 2  1: 3]` (renumbered). Use `slice`, `take`, and `drop` for subsequences.

(Historical: The old `data["b".."d"]` — range access by key value — was removed in access-pipeline-phase2.)
