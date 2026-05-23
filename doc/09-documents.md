# Documents & Pipelines

## Document Structure

A Tinct **file** contains one or more **documents** separated by `---`. Each document contains one or more **expressions**. This three-level hierarchy governs scoping, isolation, and data flow.

```text
file
├── document 1
│   ├── expression 1  (e.g., [include "utils.llt"])
│   ├── expression 2  (e.g., [x: 10  double: [fn [n] [* n 2]]])
│   └── expression 3  (e.g., [result: [double x]])
├── ---
└── document 2
    ├── expression 1
    └── expression 2
```

## Pipeline Model

Data flows through stages. Within a file, `---` separates independent documents. Each document's output becomes `%` for the next. Documents can be named with `--- %name` to allow later sections to reference them by name:

```text
file.llt
├── --- %raw                  (optional header naming the section)
├── document 1 (data)         → % for doc 2, also bound as %raw
├── ---
├── document 2 (transform)    → % for doc 3
├── ---
└── document 3 (output)       → final value, serialized by CLI
```

Within a document, sequential expressions form a scope chain — each expression's bindings are visible to the next. Only the **last** expression is the document's return value; earlier expressions exist only as scope.

## Multi-File Pipeline

The CLI accepts multiple `.llt` files as a pipeline. Each file's output becomes `%` for the next file:

```bash
# Single file (existing behavior)
tinct run config.llt

# Two-stage pipeline: data → formatter
tinct run data.llt formatter.llt

# Three-stage pipeline: data → transform → format
tinct run raw.llt transform.llt format.llt
```

This is equivalent to concatenating files with `---` separators, but allows separate files to be composed at the CLI level.

**Example:**

```tinct
# data.llt
[
  users: [
    [name: "Alice"  age: 30]
    [name: "Bob"    age: 25]
  ]
]
```

```tinct
# filter.llt
[
  adults: [filter [fn [u] [>= u.age 18]] %.users]
]
```

```bash
tinct run data.llt filter.llt
# Output: {"adults": [{"name": "Alice", "age": 30}, {"name": "Bob", "age": 25}]}
```

**Interaction with `emit`:**

When any file in the pipeline calls `emit`, the string is written directly to stdout. The final expression is still serialized to the output format (unless no `-o` flag is given). This makes `emit` purely additive — useful for logging, debugging, or producing side-channel text output alongside the main result:

```tinct
# to-yaml.llt (simplified example)
[emit [str "users:\n" [join "\n" [map [fn [u] [str "  - " u.name]] %.users]]]]
```

```bash
tinct run data.llt to-yaml.llt -o json
# Output to stdout:
# users:
#   - Alice
#   - Bob
# {"users": [{"name": "Alice", "age": 30}, {"name": "Bob", "age": 25}]}
```

Without the `-o` flag, only emit output appears (no JSON serialization).

**Pipeline semantics:**

- Each file evaluates with `%` initialized to the previous file's output
- The first file receives `%` from stdin JSON if piped, or empty dict `[]` otherwise
- Files share the same include cache — if both files include the same library, it's evaluated only once
- Each file has `%include-dir` in scope — the DirCap for the working directory; use `[include %include-dir "sibling.llt"]` to include sibling files
- The final file's output is JSON-serialized if the `-o` flag was given (default: `-o json`)

## Within a Document: Scope Chains

For an informal introduction to letrec dict scoping and scope chains, see [Evaluation](08-evaluation.md) §Recursive Dict Scoping. The formal specification with proof properties follows in §Scope Chain Semantics below.

Tinct has one scoping mechanism — lexical scope with parent chains — applied at two levels:

1. **Within a dict (letrec):** All entries in a single `[...]` share one environment. Entries can reference each other regardless of order, including mutual recursion. This is the same as Haskell's `let`/`where` or OCaml's `let rec`.

2. **Between sequential expressions:** Each expression's result dict becomes the parent scope for the next expression. Names from earlier expressions are visible but can be shadowed. Only string-keyed entries become named bindings in the scope chain; int-keyed entries remain accessible via `[get n result]` or `result.N` (integer dot access) but do not introduce variable bindings. This is analogous to a sequence of `let` blocks in ML-family languages, or nested `letrec` in Scheme.

These are not two different mechanisms. They are the same parent-chain lookup applied at different granularities. Variable lookup always walks the parent chain until it finds a match.

```text
Builtins ($+, $eval, $if, ...)
  └── Expression 1's dict (letrec within)
        └── Expression 2's dict (letrec within, sees Expr 1 via parent)
              └── Nested inner dict (sees Expr 2 via parent, Expr 1 via grandparent)
```

**Letrec within a dict:**

```tinct
[
  x: 10
  double: [fn [n] [* n 2]]
  y: [double x]            # sees x and double (same dict, letrec)
]
```

All entries share one environment. Order of definition does not matter — `y` can reference `double` even if `double` appeared after `y` in the source. This enables mutual recursion:

```tinct
[
  even?: [fn [n] [if [= n 0] true  [odd?  [- n 1]]]]
  odd?:  [fn [n] [if [= n 0] false [even? [- n 1]]]]
]
```

**Sequential expressions (scope chain):**

```tinct
# Expression 1: establishes bindings
[
  x: 10
  double: [fn [n] [* n 2]]
]

# Expression 2: sees Expression 1's bindings via parent scope
[
  y: [double x]    # x and double visible from parent
  x: 20            # shadows Expression 1's x
  z: [+ x y]       # x is 20 (local letrec), y is 20
]
```

Expression 2 creates a fresh letrec environment with Expression 1's environment as its parent. Within Expression 2, `x` resolves to the local binding (20), not Expression 1's binding (10). `double` is found by walking up to the parent.

**Module-style encapsulation:**

Because only the last expression is returned, earlier dicts act as private scope. This is the standard pattern for separating internal helpers from a public API within a single file:

```tinct
# Expression 1 — private helpers: in scope below, never exported
[
    clamp-impl: [fn@Number [lo@Number hi@Number x@Number]
        [if [< x lo] lo [if [> x hi] hi x]]]
]

# Expression 2 — public API: only value returned by eval_document
[
    clamp: [fn@Number [lo@Number hi@Number x@Number]
        [clamp-impl lo hi x]]   # clamp-impl reachable via parent scope
]
```

`include "math.llt"` returns only `[clamp: ...]`. `clamp-impl` is unreachable from outside the file. The standard library uses this pattern to keep `-impl`, `-step`, and `-check` helpers out of the user namespace.

**Nested dicts (lexical scope):**

Inner dicts see enclosing dicts' bindings by walking the parent chain. Siblings in a parent dict share one environment (letrec), so lateral access is free:

```tinct
[
  db: [host: "localhost"  port: 5432]
  cache: [
    host: "redis.local"
    # db walks up to parent, finds sibling entry
    same_host: [= host db.host]
  ]
]
```

Here, `db` inside `cache` walks up to the parent dict and finds the sibling `db` entry. `host` resolves to `"redis.local"` (the local binding shadows any outer `host`).

The builtin scope (stdlib functions like `+`, `map`, `eval`, etc.) is the root of the parent chain. Every expression's scope ultimately inherits from builtins.

### Comparison with Other Languages

| Language | Within-block scoping | Sequential scoping | Tinct equivalent |
|----------|---------------------|-------------------|----------------|
| Haskell | `where` / `let` (letrec, mutual recursion) | top-level defs (single letrec) | Dict entries |
| OCaml | `let rec ... and ...` (explicit letrec) | `let x = ... in let y = ...` (sequential) | Expr 1 then Expr 2 |
| Scheme | `letrec` (mutual visibility) | `let*` (sequential, each sees prior) | Both available |
| JavaScript | Block scope (`const`/`let`, no mutual ref) | Sequential statements | Different: JS has no letrec |
| Nix | Attribute set (`rec { }`, mutual ref) | `let ... in` (sequential) | Similar to Tinct |
| Jsonnet | Object (self/super, late binding) | No sequential model | Similar within-block |

Tinct is closest to **Nix**: `rec { }` attribute sets are letrec (mutual visibility), and `let x = ...; in` introduces sequential bindings. The key difference is that Tinct uses the same `[...]` syntax for both — a single dict is letrec, and sequential expressions in a document form a chain. There is no separate `let` keyword.

### Scope Chain Semantics — Formal Specification

Formalizes the two scoping mechanisms described above (letrec within dicts, sequential let* between expressions) using Launchbury's (1993) natural semantics for lazy evaluation, extended with Nakata & Hasegawa's (2009) cyclic call-by-need treatment for letrec cycle detection. The key insight is that both mechanisms are instances of the same primitive: `Environment::with_parent` creating a child scope linked to a parent chain.

#### Part 1: Domains and Notation

**Environments.** An environment `ρ` is a pair `(B, parent)` where `B : String → Thunk` is a finite map from names to thunks and `parent : Option<Env>` is a link to an enclosing scope. The parent chain forms a tree rooted at the builtins scope `ρ_builtins` (Property 4). The capture graph — thunks closing over their containing environment — may contain cycles in letrec scopes; see Property 4 for the distinction.

```text
ρ ::= (B, None)            — root environment (builtins)
    | (B, Some(ρ_parent))  — child environment
```

**Thunks.** Thunks follow the lifecycle specified in §Thunk Lifecycle — Formal Specification. For scoping purposes, the relevant states are `Unevaluated(expr, ρ_capture)` (closes over an environment) and `Materialized(v)` (holds a value). The `ρ_capture` in an unevaluated thunk is the environment in which the expression will be evaluated — this is how letrec mutual visibility works: all entries in a dict capture the same shared `ρ_dict`.

**Keys.** Dict entries have keys `k ∈ Key = String(s) | Int(n)`. Only `String` keys produce scope bindings; `Int` keys are positional and do not enter any environment.

**Document pipeline variable.** The variable written `%` in tinct source code appears as `%` in formal notation. `%` is an ordinary identifier (`VarRef("%")`). Named sections bind as `%name` (`VarRef("%name")`). The `Σ` (sigma) map accumulates named-section thunks across documents within a file; `Σ` is file-local and does not cross file boundaries.

**Notation conventions.** `ρ(x)` denotes lookup of name `x` in environment `ρ` (defined formally in Part 3). `ρ[x ↦ θ]` denotes extending `ρ`'s bindings with `x` bound to thunk `θ`. `dom(ρ)` is the set of names bound directly in `ρ` (not including parent bindings). `eval(e, ρ, d)` is the evaluation judgment from §Thunk Lifecycle. The rules below use an implementation-oriented notation mixing imperative state updates (`ρ.B[s] ← θ`) with declarative judgments, following the same convention as §Thunk Lifecycle — Formal Specification Part 2.

#### Part 2: Environment Construction Rules

Two rules construct environments: DICT-SCOPE for letrec within a dict, and SEQ-SCOPE for sequential expressions in a document.

**[DICT-SCOPE]** — Letrec environment for dict literals

```text
entries = [(k₁, e₁), ..., (kₙ, eₙ)]       (dict entries, keys + value exprs)
ρ_dict = ({}, Some(ρ_parent))               (fresh child env linked to parent)

∀i ∈ 1..n:
  kᵢ = eval_key(key_exprᵢ, ρ_parent, d)    (keys evaluated in PARENT scope)
  θᵢ = Unevaluated(eᵢ, ρ_dict)             (values close over SHARED dict env)
  kᵢ = String(sᵢ) ⟹ ρ_dict.B[sᵢ] ← θᵢ    (string keys become bindings)
  kᵢ = Int(_)     ⟹ no binding             (int keys are positional only)

∀i ≠ j: kᵢ ≠ kⱼ                             (duplicate keys are errors)
────────────────────────────────────────────
eval_dict(entries, ρ_parent, d) ⇒ Dict([(k₁,θ₁), ..., (kₙ,θₙ)])
```

When `entries = []` (empty dict), the quantifications over `i ∈ 1..n` are vacuous, and the rule produces `Dict([])` with `ρ_dict` containing no bindings.

The `∀i` is processed sequentially (source order). Bindings are inserted incrementally, so entry `i+1`'s thunk is created after entry `i`'s binding exists in `ρ_dict`. However, because no thunk is materialized during construction (all remain `Unevaluated` — see construction-time non-materialization invariant below), the final state of `ρ_dict` is independent of insertion order, and the sequential semantics is observationally equivalent to simultaneous binding.

**Construction-time non-materialization invariant:** No thunk in `ρ_dict` is materialized during the execution of the DICT-SCOPE `∀i` loop. `Thunk::new_unevaluated` creates thunks without materializing them, and `eval_key` evaluates in `ρ_parent` (not `ρ_dict`), so key evaluation cannot trigger materialization of sibling value thunks. Therefore, by the time any thunk is subsequently materialized, `ρ_dict.B` contains all string-keyed bindings. This is the analogue of Launchbury's (1993) heap allocation step, which adds all letrec bindings before evaluating the body.

**Key isolation invariant:** Key expressions evaluate in `ρ_parent`, not `ρ_dict`. This prevents key computation from depending on sibling values that are unevaluated thunks, ensuring key evaluation is deterministic regardless of entry order. Without this invariant, `[x: 1  [call $x]: 2]` would cause the key expression `[call $x]` to reference `x` from `ρ_dict`, creating a dependency on the sibling entry `x: 1` (an unevaluated thunk), which breaks key evaluation determinism. Key evaluation itself requires materialization of the key expression's result (to obtain a concrete `String` or `Int` key) — this is inherent materialization in the sense of §Selective Materialization, since the key's identity must be known to populate `dict_map`.

**Computed keys cannot reference sibling entries.** Because keys evaluate in `ρ_parent`, a computed key like `$k` in `[k: hello  $k: 42]` resolves `k` via `ρ_parent`, not the dict's own letrec scope `ρ_dict`. If `k` is not bound in any enclosing scope, this is an unbound-variable error. This is intentional: allowing computed keys to see the dict's own bindings would create order-dependent key evaluation (the key at position 2 depends on the binding at position 1, which hasn't been evaluated yet during key computation). The key isolation invariant is strict — no exceptions for "earlier" entries.

**Letrec sharing invariant:** All value thunks `θᵢ` capture `ρ_dict` — the same mutable environment. When any `θᵢ` is materialized, it evaluates in `ρ_dict`, which by then contains bindings for all string-keyed siblings (guaranteed by the construction-time non-materialization invariant). This is the mechanism behind mutual recursion: `even?` and `odd?` both capture the same `ρ_dict` and can reference each other through it.

**Referential integrity:** For any string-keyed entry `sᵢ ↦ θᵢ`, the thunk accessible via `lookup(sᵢ, ρ_dict)` (scope chain) and via `dict_map[String(sᵢ)]` (dict field access) is the same `Rc<Thunk>` identity (`eval.rs:348-353` uses `Rc::clone`). Materializing either access path memoizes the result for both — there is no divergence between `$x` within a dict and `.x` access on the dict from outside.

**[SEQ-SCOPE]** — Sequential expression scope chain within a document

```text
Base case:
  exprs = []                                   (empty document)
  ────────────────────────────────────────────
  eval_document([], ρ_input, d) ⇒ Materialized(Dict([]))

Recursive case:
  exprs = [e₁, ..., eₙ]                       (document expressions, n ≥ 1)
  ρ₀ = ρ_input                                (initial scope — typically builtins + %)

  ∀i ∈ 1..n-1:                                (intermediate expressions)
    θᵢ = eval(eᵢ, ρᵢ₋₁, d)
    vᵢ = materialize(θᵢ, d)                   (intermediate results are materialized)
    vᵢ = Dict(mapᵢ)                           (intermediate must be Dict — type error otherwise)
    static_keys(eᵢ) ≠ ∅  ⟹  ρᵢ = ({}, Some(ρᵢ₋₁))       (fresh child env only when there are static keys)
                              ∀(k, θ) ∈ mapᵢ:
                                k = String(s) ∧ s ∈ static_keys(eᵢ) ⟹ ρᵢ.B[s] ← θ
                                k = String(s) ∧ s ∉ static_keys(eᵢ) ⟹ no binding
                                k = Int(_)                             ⟹ no binding
    static_keys(eᵢ) = ∅  ⟹  ρᵢ = ρᵢ₋₁                   (no scope extension; no new de Bruijn level)

  θₙ = eval(eₙ, ρₙ₋₁, d)                     (last expression: lazy, any type)
  ────────────────────────────────────────────
  eval_document(exprs, ρ_input, d) ⇒ θₙ
```

**`static_keys(e)`** denotes the set of string names whose keys are syntactically static in expression `e` — specifically, dict entries with a bare-word key (`x:`) or an annotated bare-word key (`x@T:`). Keys computed at runtime (e.g., `[$k: v]` where `$k` is a variable) are excluded even if they happen to evaluate to strings. This restriction is necessary for slot-based variable resolution: the resolver assigns de Bruijn slot indices at compile time counting only static keys, so only those entries may occupy positional slots in the runtime environment. A computed-key entry (e.g., `[$k: 1]` where `k = "z"`) does not receive a slot assignment; the name `"z"` is therefore not resolvable by sibling entries via `$z`, and inserting it into the scope chain would shift the indices of all subsequent static-key entries, causing silent wrong-value bugs.

When `n = 1`, the `∀i ∈ 1..0` range is empty and the rule reduces to `eval_document([e₁], ρ_input, d) ⇒ eval(e₁, ρ_input, d)` — a single expression is evaluated lazily with no scope chain construction.

**Return value:** Only `θₙ` (the last expression's thunk) is returned. Intermediate expressions `e₁..eₙ₋₁` contribute bindings to the scope chain but are not part of the document's value. This is the formal basis for module-style encapsulation: helpers placed in earlier expressions are lexically visible within the document but are excluded from the returned value and therefore not accessible to callers.

**Intermediate materialization (strict let\* semantics):** Expressions `e₁..eₙ₋₁` are materialized to extract their dict bindings into the scope chain. This is inherent materialization — the scope chain construction itself requires knowing the dict's keys to create named bindings. Beyond extracting the dict structure, **named (string-keyed) entry values are also shallowly materialized (one-level forcing) at binding time** — this is strict `let*` semantics. Each binding's outermost thunk is forced before the next expression sees it. Consequences:

- **Dead-but-erroring bindings fail eagerly.** If a named binding computes an error, it fails at binding time even if no subsequent expression uses that name.
- **Shallow only, not deep.** The outer thunk is forced to produce a concrete `Value`; inner thunks (e.g., dict entry values) remain unevaluated. This is analogous to WHNF in call-by-need languages but is more precisely called shallow or one-level materialization in tinct's context. Use `[eval ...]` for deep materialization.
- **Use `[force expr]` for explicit control.** The `$force` builtin provides shallow materialization for function bodies and other lazy contexts where auto-materialization does not apply.

The last expression `eₙ` is returned as a lazy thunk, preserving tinct's call-by-need semantics.

**Dict-type constraint:** Intermediate expressions must evaluate to `Dict`. This is not a type system constraint (the type checker does not enforce it) but a runtime invariant. If `vᵢ` is not a `Dict`, evaluation fails with a type mismatch error.

**[DOC-PIPELINE]** — Document isolation via `%` and named sections

```text
Σ₀ = {}                                     (named-section map)
documents = [doc₁, ..., docₘ]               (file documents separated by ---)
ρ_base = ρ_builtins                          (shared root scope)
θ₀ = input_thunk                             (external input or empty dict)
d = depth                                    (evaluation depth; 0 at top-level)

∀j ∈ 1..m:
  pipeline_bindings = {% ↦ θⱼ₋₁}
                    ∪ {%n ↦ Σⱼ₋₁(n) | n ∈ dom(Σⱼ₋₁)}
  ρ_docⱼ = (pipeline_bindings, Some(ρ_base))   (fresh scope with % and %names bound)
  θⱼ = eval_document(docⱼ.exprs, ρ_docⱼ, d)
  Σⱼ = if docⱼ.name = Some(n)
       then Σⱼ₋₁[n ↦ θⱼ]
       else Σⱼ₋₁

────────────────────────────────────────────
eval_file(documents, ρ_base, input_thunk, d) ⇒ θₘ
```

The anonymous pipeline variable is `%` — the binding name is `"%"`, and `%` in source resolves to `VarRef("%")`. Named sections bind as `%name` (binding name `"%name"`). Only `%` and `%name` bindings exist — there is no `$` binding for the previous document's output. At top-level invocation `d = 0`; when called from `include` (`builtins.rs:1126`), `d = depth + 1`.

Documents are totally isolated — `ρ_docⱼ` inherits only from `ρ_base` (builtins), not from prior documents' scope chains. Data flows exclusively through pipeline bindings (`%` and `%name`). Named section bindings accumulate strictly in order — a section cannot reference its own name or a later section's name (both produce `UndefinedVariable`). Duplicate section names within a file are a parse error. A bare `%` with no following identifier on a section header (`--- %` followed by whitespace or end-of-line) is also a parse error.

**Lazy pipeline boundary:** `θⱼ₋₁` is passed without materialization. Named section thunks in `Σⱼ` are also stored as raw unevaluated thunks — the `---` boundary does not trigger materialization. The pipeline is lazy end-to-end. See Semantic Commitment 4 in §Thunk Lifecycle — Formal Specification.

#### Part 3: Variable Lookup

Variable lookup walks the parent chain from the current environment upward, returning the first match. This single mechanism implements both letrec-internal lookup and cross-expression resolution.

**[LOOKUP]**

```text
lookup(x, ρ):
  (1) x ∈ dom(ρ)         ⟹ return ρ.B[x]              (found in current scope)
  (2) ρ.parent = Some(ρ') ⟹ return lookup(x, ρ')       (recurse to parent)
  (3) ρ.parent = None     ⟹ return None                 (unbound variable)
```

The implementation (`Environment::get`, `value.rs:445-460`) converts the recursion to iteration for stack efficiency. The two formulations are equivalent because the parent chain is finite and acyclic (Property 4 below).

**Shadowing semantics:** When the same name `x` is bound in both `ρ` and an ancestor `ρ'`, clause (1) returns `ρ.B[x]` — the nearest binding wins. This is standard lexical shadowing, formalized as Property 1 below.

#### Part 4: Scope Properties

Five properties that hold for all well-formed tinct programs. Each property follows from the construction rules (Part 2) and lookup rule (Part 3). The proofs use the Launchbury (1993) heap model extended with Nakata & Hasegawa's (2009) treatment of cyclic references.

##### Property 1: Shadowing Correctness

*Statement:* If name `x` is bound in environment `ρ` at depth `d₁` and also in ancestor `ρ'` at depth `d₂ > d₁` in the parent chain, then `lookup(x, ρ)` returns `ρ`'s binding at depth `d₁`.

*Proof sketch:* By structural induction on the parent chain length. LOOKUP clause (1) returns immediately when `x ∈ dom(ρ)`, without inspecting ancestors. Since the parent chain has finite length (Property 4), the nearest binding is always reached first. The inductive step: if `x ∉ dom(ρ)`, LOOKUP recurses to `ρ.parent`, reducing the chain length by one. By the inductive hypothesis, the nearest binding in the remaining chain is returned. ∎

##### Property 2: Mutual Visibility (Letrec)

*Statement:* For a dict constructed by DICT-SCOPE with entries `{s₁, ..., sₙ}` (string keys), materializing any thunk `θᵢ` can resolve `$sⱼ` for all `j ∈ 1..n`, including `j = i`.

*Proof sketch:* By DICT-SCOPE, all `θᵢ = Unevaluated(eᵢ, ρ_dict)`. By the construction-time non-materialization invariant, no thunk is materialized during DICT-SCOPE construction, so by the time any `θᵢ` is subsequently materialized, `ρ_dict.B` contains `{s₁ ↦ θ₁, ..., sₙ ↦ θₙ}` — all string-keyed bindings are present. When `θᵢ` is materialized, `eval(eᵢ, ρ_dict, d)` has access to `ρ_dict`, and `lookup(sⱼ, ρ_dict)` succeeds via LOOKUP clause (1) for any `j`. Self-reference (`i = j`) is valid because materializing `θᵢ` transitions it to `InProgress` — a subsequent self-reference triggers MATERIALIZE-CYCLE (§Thunk Lifecycle), producing a cycle error rather than diverging. Mutual reference (`i ≠ j`) succeeds provided `θⱼ` is not already `InProgress` (no transitive cycle). This matches Nakata & Hasegawa's (2009) operational treatment of cyclic call-by-need: the `InProgress` state acts as a blackhole, ensuring termination for all reference patterns. ∎

##### Property 3: Heap Monotonicity

*Statement:* The set of bindings reachable from any environment `ρ` is monotonically non-decreasing over the course of evaluation. No binding is ever removed or reassigned to a different thunk.

*Proof sketch:* The binding map is monotonic because: (a) DICT-SCOPE rejects duplicate keys before insertion (`eval.rs:336-338`), so each binding is inserted exactly once into an initially empty map; (b) SEQ-SCOPE inserts into freshly created empty environments, so no overwrite is possible; (c) no code path calls `Environment::insert` on scope-chain environments after construction. The `insert` API itself (`IndexMap::insert`) permits overwriting, but these three invariants prevent it. The thunks themselves may transition states (Unevaluated → Materialized), but the binding `name ↦ θ` is stable — the `Rc<Thunk>` pointer does not change, only the thunk's internal state. By the thunk lifecycle monotonicity theorem (§Thunk Lifecycle Part 1), thunk state transitions are irreversible. Therefore both the binding map and the thunk contents are monotonic. ∎

##### Property 4: Scope Chain Acyclicity

*Statement:* The *parent chain* from any environment `ρ` to the root `ρ_builtins` is a finite, acyclic path.

*Proof sketch:* By induction on environment construction. Base case: `ρ_builtins` has `parent = None` — no cycle. Inductive step: both DICT-SCOPE and SEQ-SCOPE create fresh environments via `Environment::with_parent(ρ_existing)`. The new environment's parent is an already-constructed environment. Since environments are allocated with `Rc::new(RefCell::new(...))` and the parent pointer is set once at construction to an existing environment, no environment can have itself as an ancestor. Formally: define depth `d(ρ)` as the number of parent links from `ρ` to `ρ_builtins` (so `d(ρ_builtins) = 0`). DICT-SCOPE and SEQ-SCOPE both satisfy `d(ρ_new) = d(ρ_parent) + 1`, so depth strictly increases. A cycle would require `d(ρ) > d(ρ)`, a contradiction. ∎

**Parent chain vs capture graph:** This property concerns the *parent chain* (`env.parent` links), which is the graph walked by LOOKUP. The *capture graph* (`thunk.env` links) does contain cycles in letrec scopes: `ρ_dict` holds thunks that close over `ρ_dict` itself (via `Rc::clone(&dict_env)` at `eval.rs:342`). These capture cycles do not affect LOOKUP termination (LOOKUP walks only parent links) or semantic correctness. They do prevent `Rc` deallocation of letrec environments (since `Rc` cannot collect cycles), which is a known memory management limitation addressed by the arena migration (§Allocation Strategy — Phased Approach in [Evaluation](08-evaluation.md)).

##### Property 5: Determinism

*Statement:* For the pure subset of tinct (no I/O builtins such as `$include`), `eval_document(exprs, ρ, d)` produces the same result thunk for the same input tuple `(exprs, ρ, d)`, and `lookup(x, ρ)` returns the same thunk for the same name and environment.

*Proof sketch:* LOOKUP is deterministic by construction — it is a linear scan of a fixed chain with a deterministic stopping condition (first match or `None`). DICT-SCOPE processes entries in source order; key evaluation in `ρ_parent` is deterministic by induction (keys are expressions evaluated in an already-determined environment); duplicate detection is deterministic (insertion-order `IndexMap`). SEQ-SCOPE processes expressions in source order, materializing each intermediate result deterministically. The only potential source of non-determinism — letrec evaluation order — is resolved by lazy evaluation: thunks are created in source order but materialized on demand, and Ariola & Felleisen's (1997) confluence theorem (for the storeless calculus, transferred to tinct's heap model via Launchbury's (1993) adequacy result) guarantees that the materialization order does not affect the final value in the pure call-by-need calculus. Non-determinism enters only through `$include` (file system I/O), which is outside the pure subset. ∎

**Depth and MATERIALIZE-DEPTH:** Determinism holds for the full input tuple `(exprs, ρ, d)` — depth `d` is part of the input, not ambient context. The same thunk may produce different results when materialized at different depths (MATERIALIZE-DEPTH is the only materialization rule that does not transition thunk state — see Semantic Commitment 3 in §Thunk Lifecycle). This is not non-determinism but context-sensitivity: `eval_document` with a fixed `d` is a deterministic function. The CEK machine removes MAX_EVAL_DEPTH, making this caveat moot.

#### Part 5: Implementation Correspondence

The formal rules map directly to the implementation:

| Formal rule | Implementation | Source |
|------------|----------------|--------|
| DICT-SCOPE | `eval_dict()` | `eval.rs:309-352` |
| SEQ-SCOPE | `eval_document()` | `eval_pipeline.rs:33` |
| DOC-PIPELINE | `eval_file_with_input()` (binds `%` + `%name`) | `eval_pipeline.rs:256` |
| DOC-PIPELINE Σ accumulation | Named-section map `named: IndexMap<String, Rc<Thunk>>` | `eval.rs:830, 842-846, 851-853` |
| LOOKUP | `Environment::get()` | `value.rs:445-460` |
| Key isolation | `eval_key(key_expr, parent_env, d)` | `eval.rs:327` |
| String-key filter | `if let Key::String(name) = key` | `eval.rs:234, 347` |
| Letrec sharing | `Thunk::new_unevaluated(expr, dict_env)` | `eval.rs:340-344` |
| Cycle detection | `InProgress` → FORCE-CYCLE | §Thunk Lifecycle — Formal Specification, Part 2: Forcing Rules |

**Deviations from Launchbury (1993):** Launchbury's original semantics threads an explicit heap `Γ` through all judgments: `Γ : e ⇓ Γ' : v`. tinct uses mutable `Rc<RefCell<Environment>>` instead of an explicit heap, which is operationally equivalent but obscures heap threading in the formal presentation. The correspondence is: Launchbury's `Γ[x ↦ e]` = tinct's `env.borrow_mut().insert(x, thunk)`, and Launchbury's `Γ(x)` = tinct's `env.borrow().bindings.get(x)`. The mutable cell model is standard in implementations (GHC uses a similar approach via `IORef` for thunk update).

**Deviations from Nakata & Hasegawa (2009):** Nakata & Hasegawa prove that cyclic call-by-need with blackholing (InProgress detection) terminates for all terms, producing either a value or a cycle error. tinct's `InProgress` state is exactly their blackhole. The deviation is that tinct additionally caches cycle errors via the `Failed` state (§Thunk Lifecycle), which Nakata & Hasegawa do not address — their semantics re-evaluates on each access. tinct's error memoization is a conservative extension: it preserves the value/error distinction while avoiding redundant cycle detection.

**Type system parallel:** The type checker builds a parallel scope chain (`TypeEnv`) mirroring the runtime scope chain. Within a dict (letrec), bindings are monomorphic during inference — type variables are not generalized until the entire dict is checked, matching the standard restriction on polymorphic recursion in HM (§Type Inference Algorithm). Between sequential expressions, dict boundaries increment `ℓ_current` for sound let-generalization (§Let-Generalization), and type schemes (not bare types) are threaded through the sequential scope chain to preserve polymorphism across expression boundaries. The full type-level formalization is in §Type Inference Algorithm and §Let-Generalization; this spec covers only runtime scope semantics.

#### Part 6: Worked Examples

##### Example 1: Letrec mutual recursion

```tinct
[
  even?: [fn [n] [if [= n 0] true  [odd?  [- n 1]]]]
  odd?:  [fn [n] [if [= n 0] false [even? [- n 1]]]]
  result: [even? 4]
]
```

DICT-SCOPE creates `ρ_dict` with parent `ρ_builtins`:

- `ρ_dict.B = {even? ↦ θ₁, odd? ↦ θ₂, result ↦ θ₃}` where all `θᵢ = Unevaluated(eᵢ, ρ_dict)`
- Materializing `θ₃` evaluates `[even? 4]` in `ρ_dict`
- `lookup(even?, ρ_dict)` → `θ₁` (clause 1) → materializes `θ₁` → creates closure capturing `ρ_dict`
- The closure body references `odd?` → `lookup(odd?, ρ_dict)` → `θ₂` (clause 1) ✓ mutual visibility
- Evaluation terminates: `even?(4) → odd?(3) → even?(2) → odd?(1) → even?(0) → true`

##### Example 2: Sequential scope chain with shadowing

```tinct
[x: 10  double: [fn [n] [* n 2]]]

[x: 20  y: [double x]]
```

SEQ-SCOPE with `ρ₀ = ρ_builtins`:

1. Evaluate `e₁` in `ρ₀` → DICT-SCOPE creates `ρ_dict₁` with `{x ↦ θ_10, double ↦ θ_fn}`
2. Materialize → `Dict({x: θ_10, double: θ_fn})`
3. Create `ρ₁ = ({x ↦ θ_10, double ↦ θ_fn}, Some(ρ₀))`
4. Evaluate `e₂` in `ρ₁` → DICT-SCOPE creates `ρ_dict₂` with parent `ρ₁`:
   - `ρ_dict₂.B = {x ↦ θ_20, y ↦ θ_call}`
   - `lookup(double, ρ_dict₂)`: not in `ρ_dict₂` → parent `ρ₁` → found ✓ (Property 1: `x` in `ρ_dict₂` shadows `x` in `ρ₁`)
   - `lookup(x, ρ_dict₂)` → `θ_20` (local, clause 1 — shadows `ρ₁`'s `x`)
5. Return `θ_last` (the thunk for `e₂`, lazy)

**Example 3: Document pipeline with `%`**

```tinct
[base_port: 8080]

---

[port: [+ %.base_port 1]]
```

DOC-PIPELINE (with `d = 0`, `Σ₀ = {}`):

1. `pipeline_bindings₁ = {% ↦ θ_empty}`. Evaluate doc₁ → `θ₁ = Dict({base_port: θ_8080})`. `Σ₁ = {}` (no name on doc₁).
2. `pipeline_bindings₂ = {% ↦ θ₁}`. `ρ_doc₂` has NO access to `ρ_doc₁`'s bindings — `base_port` would fail. Data flows only through `%`.
3. `%.base_port` resolves: `lookup(%, ρ_doc₂)` → `θ₁`, then access chain `.base_port` on the dict.

## Between Documents: Total Isolation via `%`

`---` separates independent documents. Documents have no shared scope — as if they were in separate files.

Data flows between documents via `%`, a variable injected into each document's root scope containing the previous document's output. For the first document in a file, `%` is `[]` (empty dict). `%` is `VarRef("%")` — an ordinary identifier with no grammar special case.

**`%name` identifiers** are plain bare-word identifiers that happen to start with `%`. The `%` prefix is a convention: the CLI uses it to mark injected capability variables (`%pwd`, `%libdir`, `%stdin`, `%nc`, etc.) so they are visually distinct from user-defined variables. Named pipeline sections also bind as `%name`. User programs may define `%`-prefixed variables freely — the prefix has no special meaning to the evaluator.

**Named sections** bind a document's output as `%name` for use by all subsequent documents:

```tinct
--- %defaults
[host: "localhost"  port: 8080  workers: 4]

--- %overrides
[host: "prod.example.com"  tls: true]

---
[merge %defaults %overrides]   # multi-input: both named sections accessible
```

**`%` typing is context-dependent.** The static type of `%` varies: it is an empty closed record `[]` when no input is provided (first document, no pipeline input), or `Any` when stdin JSON is parsed via `from-json` (since the JSON shape is unknown at compile time). `[@Type %]` type assertions are the escape hatch for narrowing `%` to a specific record type. Section headers can declare input contracts with `expects:`, output types with `@Type`, and required capability types with `caps:`:

```tinct
--- %validated@ValidatedConfig expects: [name: String  port: Int] caps: [%nc: @NetCap]
[validate server-schema %]
```

```tinct
# Document 1 — % is []
[
  users: [
    [name: "Alice"  age: 30]
    [name: "Bob"    age: 25]
  ]
]
---
# Document 2 — % is Document 1's output (lazy)
[
  adults: [filter [fn [u] [>= u.age 18]] %.users]
]
---
# Document 3 — % is Document 2's output (lazy)
# Final expression is the program's output, serialized by the CLI
[eval %]
```

The `---` boundary does **not** materialize the previous document. `%` is a lazy dict — values are materialized only when accessed.

### Formal Grammar

A tinct file contains one or more documents separated by `---`. Each `---` line may carry a section header. This is the top-level grammar:

```ebnf
file          = SOI ~ document ~ (section_header ~ document)* ~ EOI
document      = expression*
expression    = !section_header ~ value
section_header = "---" ~ header_components? ~ NEWLINE
header_components = header_component+
header_component  = section_name | output_annotation | expects_pragma | caps_pragma
section_name      = "%" ~ ident_char+     // e.g., %config — bare % alone is a parse error
output_annotation = "@" ~ annotation_value
expects_pragma    = "expects" ~ ":" ~ annotation_value
caps_pragma       = "caps" ~ ":" ~ "[" ~ (cap_entry)* ~ "]"
cap_entry         = "%" ~ ident_char+ ~ ":" ~ "@" ~ ident_char+   // e.g., %nc: @NetCap
```

**File:** The outermost unit. Contains documents separated by `---` section headers.

**Document:** A sequence of expressions that form a scope chain. Each expression's result becomes the parent scope for the next expression. Documents are isolated from each other — data flows through pipeline bindings (`%` and `%name`), not the scope chain.

**Section header:** The `---` line, optionally carrying a name (`--- %config`), output type annotation (`--- %config@Config`), input contract (`--- expects: InputType`), and/or required capability declarations (`--- caps: [%nc: @NetCap  %data: @DirCap]`). All components are optional; a bare `---` is valid. A bare `%` with no identifier after it on the header line is a parse error. The components may appear in any order.

**`caps:` pragma** — declares capabilities that must be injected by the caller before this document can run. Capability types (`NetCap`, `DirCap`, etc.) are described in [Data Model](03-data-model.md) §Handles — Capability Row.

```tinct
--- caps: [%nc: @NetCap  %data: @DirCap  %store: @DirCap]
[emit [str ...]]
```

Each entry is `%name: @Type`. The type checker adds each declared cap to the TypeEnv for the document body, resolving spurious "undefined variable" errors. At runtime, the evaluator validates that each declared cap is present in the root environment and produces a clear error if not:

```text
error: %nc@NetCap is required but not provided
  inject it with:  tinct run --cap-net nc=HOST:PORT ...
  or unrestricted: tinct run --cap-net nc=any ...

error: %data@DirCap is required but not provided
  inject it with:  tinct run --cap-fs data=PATH ...

error: %config@Handle is required but not provided
  inject it with:  tinct run --cap-file config=PATH:r ...
```

Auto-injected caps (`%pwd`, `%libdir`, `%stdin`) produce a different hint if missing:

```text
error: %pwd@DirCap is required but not provided
  note: %pwd is injected automatically — did you pass --no-pwd?
```

The CLI flag name is derived from the cap name by stripping the `%` prefix: `%nc` → `--cap-net nc=...`.

**Capability type table:**

| `@Type` in `caps:` | Injected by CLI flag | Description |
|--------------------|---------------------|-------------|
| `@DirCap`          | `--cap-fs NAME=PATH` | Directory capability (read/write files under PATH) |
| `@NetCap`          | `--cap-net NAME=ENTRY` | Network capability (connect to allowed hosts) |
| `@Handle`          | `--cap-file NAME=PATH:MODE` | Pre-opened file handle (pinpoint file access) |
| `@ClockCap`        | `--cap-clock NAME` | Clock capability (real or fixed timestamp) |

**`@Handle` mode suffixes** for `--cap-file`:

- `r` — readable text handle (`$slurp` returns a String)
- `rb` — readable binary handle (`$slurp` returns Bytes)
- `w` — writable text handle (`$write-handle` writes a String)
- `wb` — writable binary handle (`$write-handle` writes Bytes)

**`doc_separator`:** Three hyphens `---` not followed by an `ident_char`. This prevents `----` or `---foo` from matching as a separator.

An empty file (or one containing only whitespace/comments) is valid and produces a file with one document containing zero expressions. An empty document produces an empty Dict `[]`.

## Include Mechanism

`include` loads, macro-expands, and evaluates a tinct file from a `DirCap`. It returns the file's last document's last value — typically a dict of named functions or constants. `include` is a tinct function defined in prelude (not a Rust builtin); it is built on the primitives `load`, `expand`, `eval-file`, and the content-addressed include cache.

**`%include-dir`:** Every included file has `%include-dir` in scope — the DirCap used to load it. This enables sub-includes without hardcoding a specific cap:

```tinct
# lib/utils.llt — uses %include-dir to include a sibling
[include %include-dir "helpers.llt"]
```

**`%` is not propagated:** Included files start with `%` = `[]` (empty dict). They do not inherit the caller's pipeline input. Include is a module-loading operation, not a pipeline stage.

**Content-addressed cache:** The include cache is keyed by `blake3(cap-identity + "|" + source)` where `cap-identity` is the directory's `(dev, ino)` filesystem identity. Identical files included via the same DirCap share one cache entry and evaluate once. Identical files at different directory paths get distinct cache entries because they may sub-include different siblings.

Two usage patterns:

**Namespaced** (like Python's `import module`):

```tinct
[
  utils: [include %include-dir "lib/utils.llt"]
  result: [utils.double 21]
]
```

**Merged into scope** (like Python's `from module import *`):

Uses the sequential-expression scope chain. The included dict becomes a scope in the parent chain:

```tinct
[include %include-dir "lib/utils.llt"]

# double is visible via parent scope
[
  result: [double 21]
]
```

Note: the merged include becomes a *parent* scope, so the included file cannot reference names defined in the local dict that follows it. This matches the semantics of other languages' merge-style imports:

| Language | Merge import | Can imported code see local names? |
|----------|-------------|-----------------------------------|
| Python | `from utils import *` | No — `utils` can't see the importer's locals |
| Nix | `with pkgs; { ... }` | No — `pkgs` attrs are fixed at definition site |
| Haskell | `import Module` | No — module was compiled independently |
| JavaScript | `import * from './utils'` | No — module has its own scope |

If the included file needs to reference local bindings, use namespaced import instead and pass values explicitly:

```tinct
[
  utils: [include %include-dir "lib/utils.llt"]
  result: [utils.make-config "localhost" 5432]
]
```

Duplicate names during merge are errors (consistent with the duplicate-keys-are-errors rule). Include cycle detection is required — even with lazy values, the scope structure must be known at include time.

### Error reporting for nested includes

When an error originates inside a chain of included files, the runtime annotates the error's stack trace with one frame per include boundary, showing the full path that led to the error:

```text
[E053] include: parse error in "bad.llt": ... (defined at ...)
  in included from outer.llt at 1:1-1:5
  in included from middle.llt at 1:1-1:24
```

Each frame reads as "`file` was included (from the enclosing context) at the given source location". Frames are ordered outermost-first: the first frame is the entry point include, the last frame is the immediate parent of the failing file. The error message itself already names the failing file (`bad.llt` above), so no redundant frame is added for it.

This chain is reconstructed dynamically from the active `$include` call stack at the time the error is raised. It reflects the actual call path, not a static import graph, so conditional includes (e.g., inside `if`) only appear in the chain when they were actually evaluated.

## Pipeline Primitives

The document pipeline and include mechanism are built on eight user-callable Rust primitives and a set of tinct functions defined in prelude.

### Rust Primitives

| Primitive | Signature | Description |
|-----------|-----------|-------------|
| `load` | `[Fn [source@String  name: @String] Dict]` | Parse source text to a file AST dict. No IO, no evaluation. `name:` is an opaque provenance hint for error spans. |
| `expand` | `[Fn [ast@Dict] Dict]` | Run macro expansion on a file AST dict; return the expanded dict. |
| `eval` | `[Fn [exprs@Dict  %: @Any  env: @Dict] Any]` | Evaluate AST expression nodes in the runtime stage env (prelude env + `%` + `env:` merge). Sequential let\* scoping: each expression's result dict extends scope for subsequent expressions. |
| `eval-types` | `[Fn [exprs@Dict] Any]` | Evaluate AST expression nodes in the type-stage env (type-level builtins only, no `%`). Used by the type checker for `--- stage: type` documents. |
| `blake3` | `[Fn [source@String] String]` | Compute blake3 hash of a string. |
| `cap-identity` | `[Fn [cap@DirCap] String]` | Return `"dev:ino"` from `fstat` on the DirCap's O_DIRECTORY fd — a stable filesystem identity for use in cache keys. |
| `include-cache-get` | `[Fn [hash@String] IncludeCacheEntry]` | Look up the content-addressed include cache by hash. |
| `include-cache-put` | `[Fn [hash@String  entry@IncludeCacheEntry] []]` | Update the include cache. |

### Tinct Pipeline Functions

These functions are defined in prelude and available to all tinct code:

- **`eval-document-pipeline`** — evaluate a file's documents, threading `%` and named sections; injects `%include-dir` into every document's scope
- **`eval-file`** — evaluate a parsed file AST dict with an explicit initial `%` and `include-dir`  
- **`include`** — load, expand, and evaluate a file from a DirCap; content-addressed memoization with circular include detection
- **`cli-pipeline`** — evaluate multiple files sequentially with `%` threading (the `tinct run` multi-file pipeline)

### `%include-dir`

Every document inside an included file has `%include-dir` in scope — the DirCap used to load the file. Use it for sub-includes:

```tinct
# lib/utils.llt — load a sibling from the same library directory
[include %include-dir "helpers.llt"]
```

`%include-dir` is injected via the `env:` parameter of `eval` and always takes precedence over scope chain promotion — a `--- %include-dir@Type` section header cannot overwrite it.

## Document Pipeline and $include — Formal Specification

This section formalizes the inter-file include mechanism. The intra-file document pipeline (`%` threading via `---` boundaries) and intra-document scope chains are already formalized in §Scope Chain Semantics — Formal Specification (DOC-PIPELINE and SEQ-SCOPE rules, respectively). This section covers `$include`: path resolution, cycle detection, result caching, and the eager materialization invariant.

### Part 1: Include State

The include system maintains mutable state `Σ` shared across nested include calls:

```text
Σ = ⟨cache, stdlib_env⟩  where
  cache      : Map<String, IncludeCacheEntry>  — content-addressed cache (see key below)
  stdlib_env : ρ                               — environment for included files (builtins + stdlib)

IncludeCacheEntry = Missing | Pending | Cached(Rc<Thunk>)
```

`Missing` means not yet loaded (or failed — reset after error so retries work). `Pending` means currently being evaluated — a second include of the same file during evaluation is a circular include error. `Cached(θ)` holds the memoized result thunk.

**Cache key:** `blake3(cap-identity + "|" + source_text)` where `cap-identity` is the `"dev:ino"` string obtained from `fstat` on the DirCap's O_DIRECTORY file descriptor. This is stable across renames and moves, correct under Linux mount namespaces (no path resolution — fd identity used directly). Same source under the same directory identity shares one cache entry; identical files at different directory paths get distinct entries because they may sub-include different siblings via `%include-dir`.

`Σ` is stored in `EvalState` (`Rc<RefCell<EvalState>>`), carried through evaluation via `Rc::clone` on `EvalContext`. Cache transitions: `Missing → Pending` (before evaluation), `Pending → Cached` (on success), `Pending → Missing` (on error, so retries work).

**Threading model:** `Σ` is threaded via `Rc<RefCell<EvalState>>` inside `EvalContext` — the `EvalContext` parameter passed through all evaluation functions. The formal semantics are independent of the threading mechanism — `Σ` transitions are the same regardless of how `Σ` is carried.

### Part 2: Path Resolution

**[RESOLVE]** — Path resolution and canonicalization:

```text
resolve(path_str, Σ.base_dir):
  raw = Path::new(path_str)
  resolved = if raw.is_absolute() then raw
             else Σ.base_dir / raw
  canonical = canonicalize(resolved)       (resolves symlinks, normalizes ..)
  ────────────────────────────────────────
  ⇒ canonical : Path
```

Canonicalization serves two purposes: (1) cycle detection requires path identity — `./lib/../lib/utils.llt` and `lib/utils.llt` must resolve to the same key; (2) caching requires the same identity guarantee. After canonicalization, the file's `(dev, ino)` identity is extracted via `std::fs::metadata` for use as the cache and cycle-detection key. Canonicalization fails with an I/O error if the path does not exist on the filesystem.

**Allowlist check:** An INCLUDE-DENY rule is inserted between RESOLVE and INCLUDE-HIT, rejecting paths outside allowed directories before consulting the cache. The check ordering is: canonicalize → allowlist → cache → cycle → read.

### Part 3: Include Rules

Three rules cover the three possible outcomes of an include call. They are checked in priority order: cache → cycle → evaluate. A fourth outcome — INCLUDE-DENY — precedes all three when the path falls outside the allowed directories.

In all rules below, `s` is the call-site span (used for error reporting but not for rule selection). The iterative evaluator has eliminated depth threading; there is no `d` parameter.

**[INCLUDE-HIT]** — Cache hit (memoized result):

```text
resolve(path_str, config.base_dir) ⇒ canonical
identity(canonical) ⇒ (dev, ino)
(dev, ino) ∈ dom(Σ.cache)
────────────────────────────────────────
include(path_str, Σ, s) ⇒ Ok(Rc::clone(Σ.cache[(dev, ino)]))
```

Cache hits return a clone of the cached thunk pointer. No file I/O, no evaluation. This is Jsonnet-style import memoization: multiple includes of the same file share a single evaluation result. The `(dev, ino)` key ensures that symlinks and hard links to the same file are correctly deduplicated.

**Cache implementation details:**

- **Cache key:** `(dev, ino)` file identity tuple (from `std::fs::metadata` after `std::fs::canonicalize`). Different relative paths, symlinks, and hard links that resolve to the same inode share a single cache entry — `./lib/utils.llt` and `subdir/../lib/utils.llt` hit the same cache key if they point to the same file.

- **Cache scope:** Stored in `EvalContext::state::include_cache` (`eval.rs:116`, `HashMap<(u64, u64), Rc<Thunk>>`). Shared via `Rc<RefCell<EvalState>>` across all nested `$include` calls within a single evaluation session.

- **Cache lifetime:** Lives as long as the `EvalContext`. In the CLI, a single `EvalContext` is created per `tinct run` invocation and cleared on exit. In the REPL, the `EvalContext` persists across REPL inputs, so included files are cached for the entire REPL session — a file modified on disk mid-session will not be re-read until the REPL is restarted.

- **Error non-caching:** Failed includes are NOT cached. If `$include("broken.llt")` fails (parse error, I/O error, eval error), subsequent `$include("broken.llt")` calls re-attempt evaluation. Only successful results populate the cache. Note that the call-site thunk caches the failure (via `ThunkState::Failed`) — the same call site will not retry — but a different call site including the same file will retry the file-level evaluation.

**[INCLUDE-CYCLE]** — Cycle detection:

```text
resolve(path_str, config.base_dir) ⇒ canonical
identity(canonical) ⇒ (dev, ino)
(dev, ino) ∉ dom(Σ.cache)
(dev, ino) ∈ Σ.guard
────────────────────────────────────────
include(path_str, Σ, s) ⇒ Err("circular include detected: {canonical}")
```

A file currently being evaluated (present in the guard set) cannot be included again. This catches direct cycles (`A includes A`) and transitive cycles (`A includes B includes A`). The error is raised at the include call site — no evaluation of the cyclic file is attempted.

**[INCLUDE-EVAL]** — Fresh evaluation:

```text
resolve(path_str, config.base_dir) ⇒ canonical
identity(canonical) ⇒ (dev, ino)
(dev, ino) ∉ dom(Σ.cache)
(dev, ino) ∉ Σ.guard
assert file_size(canonical) ≤ MAX_FILE_SIZE             (10 MB; prevents resource exhaustion)
source = read_file(canonical)                           (I/O: file read)
file = parse(source)                                    (parse tinct source)
desugar(file)                                           (AST transformation: $_ implicit lambdas)

Σ.guard ← Σ.guard ∪ {(dev, ino)}                       (push guard)
ctx' = ctx.with_base_dir(parent(canonical))             (child context for nested includes)

θ = eval_file(file, Σ.stdlib_env, ctx')                 (evaluate all documents)
v = materialize(θ, None, ctx')                          (EAGER materialization — see Part 4)

Σ.guard ← Σ.guard \ {(dev, ino)}                       (pop guard)

θ_result = Materialized(v)                              (pure allocation — no evaluation)
Σ.cache[(dev, ino)] ← θ_result                         (cache result)
────────────────────────────────────────
include(path_str, Σ, s) ⇒ Ok(θ_result)
```

On error at any step (file read, parse, eval, materialize), the guard is restored before the error propagates — the INCLUDE-RESTORE invariant (Property 3 below). The `base_dir` does not need explicit restore because it is carried in the child context `ctx'`, not mutated in `Σ`.

The iterative evaluator does not track depth, so nested includes do not consume evaluation depth. The guard set provides the cycle detection bound on include recursion.

The included file evaluates with `Σ.stdlib_env` as its root scope and `%` initialized to the empty dict (`eval_file` passes `None` as `initial_input` to `eval_file_with_input`, which defaults to `Materialized(Dict([]))`). It does *not* receive the including file's scope chain — include isolation is strict (Property 5).

### Part 4: Eager Materialization Invariant

`$include` is one of three builtins that eagerly materialize their result (the others are `$eval` and `$try`). `$include` uses single-level `materialize` (materializes the outer dict but leaves nested values as thunks), while `$eval` uses `deep_materialize` (recursively materializes all nested thunks with cycle detection). `$try` materializes the function body result to determine success or failure. The eager materialization in INCLUDE-EVAL is required for correctness of the guard-based cycle detection:

**Why not lazy?** If `$include` returned `θ` (the unevaluated result thunk) instead of `Materialized(v)`:

1. **Cycle detection breaks.** The guard entry for `canonical` is popped immediately after `eval_file` returns. A lazy result defers actual evaluation of nested `$include` calls within the result — when those deferred thunks are later materialized, `canonical` is no longer in the guard set, so a transitive cycle would go undetected.

2. **Path resolution breaks.** The `base_dir` is restored to the parent file's directory after the include returns. If the included file's result contains nested `$include` calls (as unevaluated thunks), those calls would resolve relative paths against the *parent's* `base_dir`, not the included file's directory.

3. **Cache coherence breaks.** The cached result must be a fully evaluated value so that all consumers receive semantically equivalent data. A lazy cached thunk could produce different results depending on evaluation context (depth, base_dir at the time of materialization).

Formally: eager materialization is required because the guard set and `base_dir` are stack-scoped (pushed before the call, popped after), but lazy thunks outlive their stack frame. The alternative — extending guard lifetime to match thunk lifetime — would require thunk-to-file provenance tracking that conflicts with tinct's thunk lifecycle model (thunks are anonymous after construction).

This is consistent with Nix's `import` (which also eagerly evaluates the imported expression) and Dhall's imports (which are also strict). In all three systems, the import mechanism is an intentional breach of lazy semantics required by the guard-based cycle detection model.

### Part 5: Properties

**P1 — Cycle detection termination:** The include recursion terminates for all inputs.

*Argument:* Define include depth as `n = |Σ.guard|`. INCLUDE-EVAL adds exactly one new entry to the guard set before recursing (`canonical ∉ Σ.guard` is a precondition). Each nested include either hits the cache (INCLUDE-HIT, no recursion), detects a cycle (INCLUDE-CYCLE, no recursion), or recurses with `|Σ.guard| = n + 1` (INCLUDE-EVAL). Since `Σ.guard ⊆ {canonical paths on the filesystem}` and the filesystem is finite, `n` is bounded above. Additionally, `d + 1` depth propagation means `MAX_EVAL_DEPTH` provides an independent upper bound on total recursion depth (include + evaluation combined). ∎

**P2 — Cache determinism:** For a fixed filesystem state, `include(path, Σ, d, s)` returns the same result for the same canonical path, regardless of which call site triggered the first evaluation.

*Argument:* INCLUDE-HIT returns `Rc::clone(Σ.cache[canonical])` — a shared pointer to the first evaluation's result. INCLUDE-EVAL evaluates the file exactly once per canonical path (subsequent calls hit the cache). The cached value is `Materialized(v)` (eager), so no further evaluation occurs. The first evaluation is deterministic for a fixed filesystem state (by Property 5 in §Scope Chain Semantics — determinism of the pure subset).

**Failure non-caching:** Failed includes do NOT populate `Σ.cache` — only successful results are cached. A failed `$include("lib.llt")` from call site A does not prevent call site B from re-attempting the same file. Under a fixed filesystem state, the re-attempt produces the same error (determinism holds). Note the two caching levels operate independently: the *include cache* (`Σ.cache`) does not remember failures, but each *call-site thunk* caches its failure permanently via `Failed` state (Semantic Commitment 1). ∎

**P3 — Guard restoration (INCLUDE-RESTORE):** The include guard and `base_dir` are always restored to their pre-call state, even when evaluation fails.

*Correspondence:* `builtins.rs` — the `cleanup()` closure removes the canonical path from the include guard and pops the include chain entry. The `materialize()` call is wrapped in a `match` statement with `cleanup()` explicitly called in both the `Ok` branch and the `Err` branch. This ensures that a failed include does not leave stale entries in the guard set (which would cause false cycle-detection errors for subsequent includes of the same file from different call sites). The guard and chain inserts are placed after all fallible `open_dir` operations, so cleanup is only required once the guard/chain have been pushed.

**Cleanup safety:** The `materialize` call is wrapped in an explicit `match` with the `cleanup()` closure invoked in both the `Ok` and `Err` branches, ensuring a failed include never leaves stale entries in the guard set (which would cause false cycle-detection errors for subsequent includes of the same file from different call sites).

**P4 — Include determinism (conditional):** For a fixed filesystem state, the document pipeline `eval_file(file, ρ, d)` is deterministic. When the filesystem changes between evaluations, results may differ — `$include` is the sole source of nondeterminism in tinct (see §Thunk Lifecycle — Semantic Properties, Determinism; also Semantic Commitment 2 in §Thunk Lifecycle — Semantic Commitments).

**P5 — Include isolation:** An included file has no access to the including file's scope chain. Included files evaluate in `Σ.stdlib_env` (builtins + stdlib only), with `%` initialized to the empty dict:

```text
include(path, Σ, d, s):
  eval_file(file, Σ.stdlib_env, d + 1)     ← stdlib env, not caller's env
```

This matches the document isolation property of DOC-PIPELINE (§Scope Chain Semantics Part 2): included files are semantically equivalent to the first document in a standalone file. Data must flow through the include result, not through shared scope:

```tinct
# Namespaced: included file returns a dict, caller accesses its bindings
[utils: [include "lib/utils.llt"]
 result: [utils.double 21]]

# Merged: included file's dict becomes a parent scope via SEQ-SCOPE
[include "lib/utils.llt"]
[result: [double 21]]
```

### Part 6: Implementation Correspondence

| Spec element | Implementation |
|-------------|----------------|
| Σ (EvalState) | `eval.rs:109-144` (`include_guard`, `include_cache`, `include_chain`, `eval_stack`) |
| Σ context | `EvalContext` (`eval.rs:52-55`) |
| RESOLVE | `builtins.rs:1248-1260` (resolve + canonicalize) |
| INCLUDE-HIT | `builtins.rs:1263-1265` (cache lookup + Rc::clone) |
| INCLUDE-CYCLE | `builtins.rs:1268-1270` (guard check) |
| INCLUDE-EVAL | `builtins.rs:1231-1357` (read, parse, desugar, guard push, eval, materialize, cache) |
| `desugar(file)` | `builtins.rs:1297` (`desugar_file(&mut file.node)`) |
| Eager materialization | `builtins.rs:1331` (`materialize(&thunk, None, &included_ctx, depth + 1)`) |
| Guard push | `builtins.rs:1300-1303` (`include_guard.insert`) |
| Guard pop + base_dir restore | `builtins.rs:1323` (`cleanup` closure) |
| Cache store | `builtins.rs:1345-1348` |
| DOC-PIPELINE (cross-ref) | `eval_file_with_input` (`eval_pipeline.rs:256`) |
| SEQ-SCOPE (cross-ref) | `eval_document` (`eval_pipeline.rs:33`) |

## Pure Language, CLI Handles I/O

Tinct is a pure data transformation language with no in-language side effects, modulo `$include`, which performs filesystem I/O as a controlled side effect with sandboxing (similar to Nix's `import` and Dhall's `import`). The program evaluates to a value; the CLI serializes it:

```sh
tinct run file.llt              # evaluate, output result as JSON
tinct run --eval file.llt       # deep-materialize all thunks before serializing (surfaces errors before partial output)
tinct run -                     # read Tinct source from stdin
cat data.json | tinct run file.llt  # stdin JSON parsed and injected as % for first document
```

**Default output formatter:** The JSON output produced by `tinct run` is generated by `stdlib/out/json.llt` — a pure-tinct JSON serializer that lives in the standard library. This formatter is user-visible: you can inspect it, customize it, or use it directly in your own programs via `[include libdir "out/json.llt"]`. If `stdlib/out/json.llt` is not found (e.g. when running the binary without the stdlib installed), the CLI falls back to a built-in Rust serializer. The output is indented (2-space pretty-printed) by default.

This is the Jsonnet/Nix model: the language produces data, an external tool handles I/O. Unreferenced dict entries are never computed. There is no `$write`, `$read`, `$stdout`, `$stdin`, or channel system.

`$eval` is a runtime-supported function that recursively materializes all thunks in its argument. It performs full materialization: the entire structure is materialized in memory. The implementation caps recursion at depth 256 and returns an error if exceeded. On infinite or cyclic structures, `$eval` will hit the depth limit rather than diverging. Use `$take` to bound infinite sequences before passing them to `$eval`.

```tinct
# Without eval: CLI serializes lazily (streaming, may partially output then hit an error)
[result: [map %.data [fn [x] [+ x 1]]]]

# With eval: everything materialized in memory first (errors caught before any output)
[result: [eval [map %.data [fn [x] [+ x 1]]]]]

# Safe on infinite sequences: take bounds before eval
[result: [eval [take 100 %.sequence]]]
```

**Why pure?** In-language I/O in a lazy language creates a materialization problem: side-effecting expressions buried in lazy dict entries may never execute, and execution order becomes unpredictable. By making the language pure, lazy evaluation is semantically transparent — the result is the same regardless of evaluation order. The CLI is the only I/O boundary, and it materializes exactly what it needs to serialize the output.

**Security:** External input (stdin, files) is parsed by the CLI and injected as structured data (`%`). The language never evaluates untrusted input as code. `$from-json` is a pure function that converts a JSON string to a dict — safe on untrusted input.

## Literate Mode

`tinct literate` processes Markdown files containing embedded tinct code blocks. This enables executable documentation: prose and code co-located in a single Markdown file, where the code blocks form a pipeline that can be extracted and evaluated.

### Usage

```bash
tinct literate tangle file.md   # extract code blocks, print as ---‑separated pipeline
tinct literate eval   file.md   # extract blocks, evaluate pipeline, print JSON
tinct literate weave  file.md   # evaluate blocks, annotate Markdown with results
```

### Code Block Recognition

Fenced code blocks tagged with `tinct` or `llt` are recognized as tinct code:

````markdown
```tinct
[port: 8080  workers: 4]
```
````

````markdown
```llt
[port: 8080  workers: 4]
```
````

Other fenced blocks (`` ```rust ``, `` ```yaml ``, etc.) are ignored.

### Pipeline Semantics

Each code block is a pipeline stage, equivalent to one document in a `---`-separated `.llt` file. `%` threads between blocks in document order — the output of block N becomes `%` for block N+1.

**Example:**

````markdown
# Server Configuration

Base configuration:

```tinct
[port: 8080  workers: 4]
```

Scale workers to 2x for production:

```tinct
[port: %.port  workers: [* %.workers 2]]
```
````

```bash
tinct literate eval config.md
# Output: {"port": 8080, "workers": 8}
```

### Tangle Mode

`tangle` extracts code blocks and prints them joined with `\n---\n` separators. The output is valid tinct source that can be piped into `tinct run -` or redirected to a `.llt` file:

```bash
tinct literate tangle config.md
# Output:
# [port: 8080  workers: 4]
#
# ---
# [port: %.port  workers: [* %.workers 2]]
```

### Eval Mode

`eval` is equivalent to `tangle` followed by `tinct run`. Extracts blocks, joins them, evaluates the resulting pipeline, and prints JSON.

If no tinct code blocks are found in the Markdown file, `eval` exits with an error.

### Weave Mode

`weave` evaluates each block in pipeline order and outputs the original Markdown with the JSON result appended as an HTML comment immediately after each closing fence:

````markdown
# Config

```tinct
[port: 8080]
```
<!-- tinct-result: {"port": 8080} -->
````

The result at each block is the intermediate pipeline value at that point — the output of that block after receiving `%` from all preceding blocks. Full result substitution replaces the inline markers in prose with serialized values.

If the Markdown file contains no tinct blocks, `weave` outputs the file unchanged.

### Interaction with `emit`

Literate mode composes with the `emit` builtin. If a code block calls `emit`, the string is written to stdout and the final expression is still serialized to JSON as the block's result. The `emit` output appears in stdout alongside the JSON annotation in the weaved output.

### Base Directory

For `$include` resolution within literate code blocks, the base directory is the directory containing the Markdown file (not the current working directory). This matches the behavior of `tinct run file.llt`.

### Formal Relationship to `---` Pipeline

`tinct literate eval file.md` is semantically equivalent to:

```bash
tinct literate tangle file.md | tinct run -
```

The Markdown extraction is a preprocessing pass that produces a tinct source string with `---` separators. The existing parser and evaluator handle the rest unchanged. No new evaluation semantics are introduced — literate mode is purely a source-level transformation.
