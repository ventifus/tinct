# Documents & Pipelines

## Pipeline Model

Data flows through stages. Within a file, `---` separates independent documents. Each document's output becomes `%` for the next. Documents can be named with `--- %name` to allow later sections to reference them by name:

    file.llt
    ├── --- %raw                  (optional header naming the section)
    ├── document 1 (data)         → % for doc 2, also bound as %raw
    ├── ---
    ├── document 2 (transform)    → % for doc 3
    ├── ---
    └── document 3 (output)       → final value, serialized by CLI

Within a document, sequential expressions form a scope chain — each expression's bindings are visible to the next.

## Multi-File Pipeline

The CLI accepts multiple `.llt` files as a pipeline. Each file's output becomes `%` for the next file:

```bash
# Single file (existing behavior)
tinct eval config.llt

# Two-stage pipeline: data → formatter
tinct eval data.llt formatter.llt

# Three-stage pipeline: data → transform → format
tinct eval raw.llt transform.llt format.llt
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
tinct eval data.llt filter.llt
# Output: {"adults": [{"name": "Alice", "age": 30}, {"name": "Bob", "age": 25}]}
```

**Interaction with `emit`:**

When any file in the pipeline calls `emit`, the string is written directly to stdout and the default JSON output is suppressed. This enables text-based formatters:

```tinct
# to-yaml.llt (simplified example)
[emit [str "users:\n" [join "\n" [map [fn [u] [str "  - " u.name]] %.users]]]]
```

```bash
tinct eval data.llt to-yaml.llt
# Output:
# users:
#   - Alice
#   - Bob
```

**Pipeline semantics:**

- Each file evaluates with `%` initialized to the previous file's output
- The first file receives `%` from stdin JSON if piped, or empty dict `[]` otherwise
- Files share the same include cache — if both files include the same library, it's evaluated only once
- Each file's `$include` calls resolve relative to that file's directory
- The final file's output is JSON-serialized unless `emit` was called

## Document Structure

A Tinct **file** contains one or more **documents** separated by `---`. Each document contains one or more **expressions**. This three-level hierarchy governs scoping, isolation, and data flow.

```
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

## Within a Document: Scope Chains

Tinct has one scoping mechanism -- lexical scope with parent chains -- applied at two levels:

1. **Within a dict (letrec):** All entries in a single `[...]` share one environment. Entries can reference each other regardless of order, including mutual recursion. This is the same as Haskell's `let`/`where` or OCaml's `let rec`.

2. **Between sequential expressions:** Each expression's result dict becomes the parent scope for the next expression. Names from earlier expressions are visible but can be shadowed. Only string-keyed entries become named bindings in the scope chain; int-keyed entries remain accessible via bracket access on the result but do not introduce variable bindings. This is analogous to a sequence of `let` blocks in ML-family languages, or nested `letrec` in Scheme.

These are not two different mechanisms. They are the same parent-chain lookup applied at different granularities. Variable lookup always walks the parent chain until it finds a match.

```
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

All entries share one environment. Order of definition does not matter -- `y` can reference `double` even if `double` appeared after `y` in the source. This enables mutual recursion:

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

Tinct is closest to **Nix**: `rec { }` attribute sets are letrec (mutual visibility), and `let x = ...; in` introduces sequential bindings. The key difference is that Tinct uses the same `[...]` syntax for both -- a single dict is letrec, and sequential expressions in a document form a chain. There is no separate `let` keyword.

### Scope Chain Semantics — Formal Specification

Formalizes the two scoping mechanisms described above (letrec within dicts, sequential let* between expressions) using Launchbury's (1993) natural semantics for lazy evaluation, extended with Nakata & Hasegawa's (2009) cyclic call-by-need treatment for letrec cycle detection. The key insight is that both mechanisms are instances of the same primitive: `Environment::with_parent` creating a child scope linked to a parent chain.

#### Part 1: Domains and Notation

**Environments.** An environment `ρ` is a pair `(B, parent)` where `B : String → Thunk` is a finite map from names to thunks and `parent : Option<Env>` is a link to an enclosing scope. The parent chain forms a tree rooted at the builtins scope `ρ_builtins` (Property 4). The capture graph — thunks closing over their containing environment — may contain cycles in letrec scopes; see Property 4 for the distinction.

```
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

```
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

The `∀i` is processed sequentially (source order). Bindings are inserted incrementally, so entry `i+1`'s thunk is created after entry `i`'s binding exists in `ρ_dict`. However, because no thunk is forced during construction (all remain `Unevaluated` — see construction-time non-forcing invariant below), the final state of `ρ_dict` is independent of insertion order, and the sequential semantics is observationally equivalent to simultaneous binding.

**Construction-time non-forcing invariant:** No thunk in `ρ_dict` is forced during the execution of the DICT-SCOPE `∀i` loop. `Thunk::new_unevaluated` creates thunks without forcing them, and `eval_key` evaluates in `ρ_parent` (not `ρ_dict`), so key evaluation cannot trigger forcing of sibling value thunks. Therefore, by the time any thunk is subsequently forced, `ρ_dict.B` contains all string-keyed bindings. This is the analogue of Launchbury's (1993) heap allocation step, which adds all letrec bindings before evaluating the body.

**Key isolation invariant:** Key expressions evaluate in `ρ_parent`, not `ρ_dict`. This prevents key computation from depending on sibling values that are unevaluated thunks, ensuring key evaluation is deterministic regardless of entry order. Without this invariant, `[x: 1  [call $x]: 2]` would cause the key expression `[call $x]` to reference `x` from `ρ_dict`, creating a dependency on the sibling entry `x: 1` (an unevaluated thunk), which breaks key evaluation determinism. Key evaluation itself requires materialization of the key expression's result (to obtain a concrete `String` or `Int` key) — this is inherent materialization in the sense of §Selective Materialization, since the key's identity must be known to populate `dict_map`.

**Computed keys cannot reference sibling entries.** Because keys evaluate in `ρ_parent`, a computed key like `$k` in `[k: hello  $k: 42]` resolves `k` via `ρ_parent`, not the dict's own letrec scope `ρ_dict`. If `k` is not bound in any enclosing scope, this is an unbound-variable error. This is intentional: allowing computed keys to see the dict's own bindings would create order-dependent key evaluation (the key at position 2 depends on the binding at position 1, which hasn't been evaluated yet during key computation). The key isolation invariant is strict — no exceptions for "earlier" entries.

**Letrec sharing invariant:** All value thunks `θᵢ` capture `ρ_dict` — the same mutable environment. When any `θᵢ` is forced, it evaluates in `ρ_dict`, which by then contains bindings for all string-keyed siblings (guaranteed by the construction-time non-forcing invariant). This is the mechanism behind mutual recursion: `even?` and `odd?` both capture the same `ρ_dict` and can reference each other through it.

**Referential integrity:** For any string-keyed entry `sᵢ ↦ θᵢ`, the thunk accessible via `lookup(sᵢ, ρ_dict)` (scope chain) and via `dict_map[String(sᵢ)]` (dict field access) is the same `Rc<Thunk>` identity (`eval.rs:348-353` uses `Rc::clone`). Forcing either access path memoizes the result for both — there is no divergence between `$x` within a dict and `.x` access on the dict from outside.

**[SEQ-SCOPE]** — Sequential expression scope chain within a document

```
Base case:
  exprs = []                                   (empty document)
  ────────────────────────────────────────────
  eval_document([], ρ_input, d) ⇒ Materialized(Dict([]))

Recursive case:
  exprs = [e₁, ..., eₙ]                       (document expressions, n ≥ 1)
  ρ₀ = ρ_input                                (initial scope — typically builtins + %)

  ∀i ∈ 1..n-1:                                (intermediate expressions)
    θᵢ = eval(eᵢ, ρᵢ₋₁, d)
    vᵢ = force(θᵢ, d)                         (intermediate results are materialized)
    vᵢ = Dict(mapᵢ)                           (intermediate must be Dict — type error otherwise)
    ρᵢ = ({}, Some(ρᵢ₋₁))                    (fresh child env linked to prior scope)
    ∀(k, θ) ∈ mapᵢ:
      k = String(s) ⟹ ρᵢ.B[s] ← θ           (string keys become bindings)
      k = Int(_)    ⟹ no binding              (int keys are positional only)

  θₙ = eval(eₙ, ρₙ₋₁, d)                     (last expression: lazy, any type)
  ────────────────────────────────────────────
  eval_document(exprs, ρ_input, d) ⇒ θₙ
```

When `n = 1`, the `∀i ∈ 1..0` range is empty and the rule reduces to `eval_document([e₁], ρ_input, d) ⇒ eval(e₁, ρ_input, d)` — a single expression is evaluated lazily with no scope chain construction.

**Intermediate materialization:** Expressions `e₁..eₙ₋₁` are forced to extract their dict bindings into the scope chain. This is inherent materialization — the scope chain construction itself requires knowing the dict's keys to create named bindings. Note that the thunks `θ` extracted from `mapᵢ` are inserted into `ρᵢ` *without further materialization* — only the dict structure is forced, not the individual entry values. Those values remain lazy and are forced only when accessed via `$name` in subsequent expressions. The last expression `eₙ` is returned as a lazy thunk, preserving tinct's call-by-need semantics.

**Dict-type constraint:** Intermediate expressions must evaluate to `Dict`. This is not a type system constraint (the type checker does not enforce it) but a runtime invariant. If `vᵢ` is not a `Dict`, evaluation fails with a type mismatch error.

**[DOC-PIPELINE]** — Document isolation via `%` and named sections

```
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

The anonymous pipeline variable is `%` — the binding name is `"%"`, and `%` in source resolves to `VarRef("%")`. Named sections bind as `%name` (binding name `"%name"`). Note: the earlier syntax included a `$` binding for the previous document's output; this was removed in the new-syntax-a sprint — only `%` and `%name` bindings exist now. At top-level invocation `d = 0`; when called from `include` (`builtins.rs:1126`), `d = depth + 1`.

Documents are totally isolated — `ρ_docⱼ` inherits only from `ρ_base` (builtins), not from prior documents' scope chains. Data flows exclusively through pipeline bindings (`%` and `%name`). Named section bindings accumulate strictly in order — a section cannot reference its own name or a later section's name (both produce `UndefinedVariable`). Duplicate section names within a file are a parse error. A bare `%` with no following identifier on a section header (`--- %` followed by whitespace or end-of-line) is also a parse error.

**Lazy pipeline boundary:** `θⱼ₋₁` is passed without materialization. Named section thunks in `Σⱼ` are also stored as raw unevaluated thunks — the `---` boundary does not force evaluation. The pipeline is lazy end-to-end. See Semantic Commitment 4 in §Thunk Lifecycle — Formal Specification.

#### Part 3: Variable Lookup

Variable lookup walks the parent chain from the current environment upward, returning the first match. This single mechanism implements both letrec-internal lookup and cross-expression resolution.

**[LOOKUP]**

```
lookup(x, ρ):
  (1) x ∈ dom(ρ)         ⟹ return ρ.B[x]              (found in current scope)
  (2) ρ.parent = Some(ρ') ⟹ return lookup(x, ρ')       (recurse to parent)
  (3) ρ.parent = None     ⟹ return None                 (unbound variable)
```

The implementation (`Environment::get`, `value.rs:445-460`) converts the recursion to iteration for stack efficiency. The two formulations are equivalent because the parent chain is finite and acyclic (Property 4 below).

**Shadowing semantics:** When the same name `x` is bound in both `ρ` and an ancestor `ρ'`, clause (1) returns `ρ.B[x]` — the nearest binding wins. This is standard lexical shadowing, formalized as Property 1 below.

#### Part 4: Scope Properties

Five properties that hold for all well-formed tinct programs. Each property follows from the construction rules (Part 2) and lookup rule (Part 3). The proofs use the Launchbury (1993) heap model extended with Nakata & Hasegawa's (2009) treatment of cyclic references.

**Property 1: Shadowing Correctness**

*Statement:* If name `x` is bound in environment `ρ` at depth `d₁` and also in ancestor `ρ'` at depth `d₂ > d₁` in the parent chain, then `lookup(x, ρ)` returns `ρ`'s binding at depth `d₁`.

*Proof sketch:* By structural induction on the parent chain length. LOOKUP clause (1) returns immediately when `x ∈ dom(ρ)`, without inspecting ancestors. Since the parent chain has finite length (Property 4), the nearest binding is always reached first. The inductive step: if `x ∉ dom(ρ)`, LOOKUP recurses to `ρ.parent`, reducing the chain length by one. By the inductive hypothesis, the nearest binding in the remaining chain is returned. ∎

**Property 2: Mutual Visibility (Letrec)**

*Statement:* For a dict constructed by DICT-SCOPE with entries `{s₁, ..., sₙ}` (string keys), forcing any thunk `θᵢ` can resolve `$sⱼ` for all `j ∈ 1..n`, including `j = i`.

*Proof sketch:* By DICT-SCOPE, all `θᵢ = Unevaluated(eᵢ, ρ_dict)`. By the construction-time non-forcing invariant, no thunk is forced during DICT-SCOPE construction, so by the time any `θᵢ` is subsequently forced, `ρ_dict.B` contains `{s₁ ↦ θ₁, ..., sₙ ↦ θₙ}` — all string-keyed bindings are present. When `θᵢ` is forced, `eval(eᵢ, ρ_dict, d)` has access to `ρ_dict`, and `lookup(sⱼ, ρ_dict)` succeeds via LOOKUP clause (1) for any `j`. Self-reference (`i = j`) is valid because forcing `θᵢ` transitions it to `InProgress` — a subsequent self-reference triggers FORCE-CYCLE (§Thunk Lifecycle), producing a cycle error rather than diverging. Mutual reference (`i ≠ j`) succeeds provided `θⱼ` is not already `InProgress` (no transitive cycle). This matches Nakata & Hasegawa's (2009) operational treatment of cyclic call-by-need: the `InProgress` state acts as a blackhole, ensuring termination for all reference patterns. ∎

**Property 3: Heap Monotonicity**

*Statement:* The set of bindings reachable from any environment `ρ` is monotonically non-decreasing over the course of evaluation. No binding is ever removed or reassigned to a different thunk.

*Proof sketch:* The binding map is monotonic because: (a) DICT-SCOPE rejects duplicate keys before insertion (`eval.rs:336-338`), so each binding is inserted exactly once into an initially empty map; (b) SEQ-SCOPE inserts into freshly created empty environments, so no overwrite is possible; (c) no code path calls `Environment::insert` on scope-chain environments after construction. The `insert` API itself (`IndexMap::insert`) permits overwriting, but these three invariants prevent it. The thunks themselves may transition states (Unevaluated → Materialized), but the binding `name ↦ θ` is stable — the `Rc<Thunk>` pointer does not change, only the thunk's internal state. By the thunk lifecycle monotonicity theorem (§Thunk Lifecycle Part 1), thunk state transitions are irreversible. Therefore both the binding map and the thunk contents are monotonic. ∎

**Property 4: Scope Chain Acyclicity**

*Statement:* The *parent chain* from any environment `ρ` to the root `ρ_builtins` is a finite, acyclic path.

*Proof sketch:* By induction on environment construction. Base case: `ρ_builtins` has `parent = None` — no cycle. Inductive step: both DICT-SCOPE and SEQ-SCOPE create fresh environments via `Environment::with_parent(ρ_existing)`. The new environment's parent is an already-constructed environment. Since environments are allocated with `Rc::new(RefCell::new(...))` and the parent pointer is set once at construction to an existing environment, no environment can have itself as an ancestor. Formally: define depth `d(ρ)` as the number of parent links from `ρ` to `ρ_builtins` (so `d(ρ_builtins) = 0`). DICT-SCOPE and SEQ-SCOPE both satisfy `d(ρ_new) = d(ρ_parent) + 1`, so depth strictly increases. A cycle would require `d(ρ) > d(ρ)`, a contradiction. ∎

**Parent chain vs capture graph:** This property concerns the *parent chain* (`env.parent` links), which is the graph walked by LOOKUP. The *capture graph* (`thunk.env` links) does contain cycles in letrec scopes: `ρ_dict` holds thunks that close over `ρ_dict` itself (via `Rc::clone(&dict_env)` at `eval.rs:342`). These capture cycles do not affect LOOKUP termination (LOOKUP walks only parent links) or semantic correctness. They do prevent `Rc` deallocation of letrec environments (since `Rc` cannot collect cycles), which is a known memory management limitation addressed by the arena migration (§Allocation Strategy — Phased Approach in [Evaluation](08-evaluation.md)).

**Property 5: Determinism**

*Statement:* For the pure subset of tinct (no I/O builtins such as `$include`), `eval_document(exprs, ρ, d)` produces the same result thunk for the same input tuple `(exprs, ρ, d)`, and `lookup(x, ρ)` returns the same thunk for the same name and environment.

*Proof sketch:* LOOKUP is deterministic by construction — it is a linear scan of a fixed chain with a deterministic stopping condition (first match or `None`). DICT-SCOPE processes entries in source order; key evaluation in `ρ_parent` is deterministic by induction (keys are expressions evaluated in an already-determined environment); duplicate detection is deterministic (insertion-order `IndexMap`). SEQ-SCOPE processes expressions in source order, materializing each intermediate result deterministically. The only potential source of non-determinism — letrec evaluation order — is resolved by lazy evaluation: thunks are created in source order but forced on demand, and Ariola & Felleisen's (1997) confluence theorem (for the storeless calculus, transferred to tinct's heap model via Launchbury's (1993) adequacy result) guarantees that the order of forcing does not affect the final value in the pure call-by-need calculus. Non-determinism enters only through `$include` (file system I/O), which is outside the pure subset. ∎

**Depth and FORCE-DEPTH:** Determinism holds for the full input tuple `(exprs, ρ, d)` — depth `d` is part of the input, not ambient context. The same thunk may produce different results when forced at different depths (FORCE-DEPTH is the only forcing rule that does not transition thunk state — see Semantic Commitment 3 in §Thunk Lifecycle). This is not non-determinism but context-sensitivity: `eval_document` with a fixed `d` is a deterministic function. The CEK machine removes MAX_EVAL_DEPTH, making this caveat moot.

#### Part 5: Implementation Correspondence

The formal rules map directly to the implementation:

| Formal rule | Implementation | Source |
|------------|----------------|--------|
| DICT-SCOPE | `eval_dict()` | `eval.rs:309-352` |
| SEQ-SCOPE | `eval_document()` | `eval.rs:199-249` |
| DOC-PIPELINE | `eval_file_with_input()` (binds `%` + `%name`) | `eval.rs:820-859` |
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

**Example 1: Letrec mutual recursion**

```tinct
[
  even?: [fn [n] [if [= n 0] true  [odd?  [- n 1]]]]
  odd?:  [fn [n] [if [= n 0] false [even? [- n 1]]]]
  result: [even? 4]
]
```

DICT-SCOPE creates `ρ_dict` with parent `ρ_builtins`:
- `ρ_dict.B = {even? ↦ θ₁, odd? ↦ θ₂, result ↦ θ₃}` where all `θᵢ = Unevaluated(eᵢ, ρ_dict)`
- Forcing `θ₃` evaluates `[even? 4]` in `ρ_dict`
- `lookup(even?, ρ_dict)` → `θ₁` (clause 1) → forces `θ₁` → creates closure capturing `ρ_dict`
- The closure body references `odd?` → `lookup(odd?, ρ_dict)` → `θ₂` (clause 1) ✓ mutual visibility
- Evaluation terminates: `even?(4) → odd?(3) → even?(2) → odd?(1) → even?(0) → true`

**Example 2: Sequential scope chain with shadowing**

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

**Named sections** bind a document's output as `%name` for use by all subsequent documents:

```tinct
--- %defaults
[host: "localhost"  port: 8080  workers: 4]

--- %overrides
[host: "prod.example.com"  tls: true]

---
[merge %defaults %overrides]   # multi-input: both named sections accessible
```

**`%` typing is context-dependent.** The static type of `%` varies: it is an empty closed record `[]` when no input is provided (first document, no pipeline input), or `Any` when stdin JSON is parsed via `from-json` (since the JSON shape is unknown at compile time). `[@Type %]` type assertions are the escape hatch for narrowing `%` to a specific record type. Section headers can declare input contracts with `expects:` and output types with `@Type`:

```tinct
--- %validated@ValidatedConfig expects: [name: String  port: Int]
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
header_component  = section_name | output_annotation | expects_pragma
section_name      = "%" ~ ident_char+     // e.g., %config — bare % alone is a parse error
output_annotation = "@" ~ annotation_value
expects_pragma    = "expects" ~ ":" ~ annotation_value
```

**File:** The outermost unit. Contains documents separated by `---` section headers.

**Document:** A sequence of expressions that form a scope chain. Each expression's result becomes the parent scope for the next expression. Documents are isolated from each other — data flows through pipeline bindings (`%` and `%name`), not the scope chain.

**Section header:** The `---` line, optionally carrying a name (`--- %config`), output type annotation (`--- %config@Config`), and/or input contract (`--- expects: InputType`). All components are optional; a bare `---` is valid. A bare `%` with no identifier after it on the header line is a parse error. The components may appear in any order — the parser does not enforce a fixed sequence. The conventional order is `%name@OutputType expects: InputType`, but `--- expects: T %name` is equally valid.

**`doc_separator`:** Three hyphens `---` not followed by an `ident_char`. This prevents `----` or `---foo` from matching as a separator.

An empty file (or one containing only whitespace/comments) is valid and produces a file with one document containing zero expressions. An empty document produces an empty Dict `[]`.

## Include Mechanism

`include` evaluates a file and returns its dict. Two usage patterns:

**Namespaced** (like Python's `import module`):

```tinct
[
  utils: [include "lib/utils.llt"]
  result: [utils.double 21]
]
```

**Merged into scope** (like Python's `from module import *`):

Uses the sequential-expression scope chain. The included dict becomes a scope in the parent chain:

```tinct
[include "lib/utils.llt"]

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
  utils: [include "lib/utils.llt"]
  result: [utils.make-config "localhost" 5432]
]
```

Duplicate names during merge are errors (consistent with the duplicate-keys-are-errors rule). Include cycle detection is required — even with lazy values, the scope structure must be known at include time.

### Error reporting for nested includes

When an error originates inside a chain of included files, the runtime annotates the error's stack trace with one frame per include boundary, showing the full path that led to the error:

```
[E053] include: parse error in "bad.llt": ... (defined at ...)
  in included from outer.llt at 1:1-1:5
  in included from middle.llt at 1:1-1:24
```

Each frame reads as "`file` was included (from the enclosing context) at the given source location". Frames are ordered outermost-first: the first frame is the entry point include, the last frame is the immediate parent of the failing file. The error message itself already names the failing file (`bad.llt` above), so no redundant frame is added for it.

This chain is reconstructed dynamically from the active `$include` call stack at the time the error is raised. It reflects the actual call path, not a static import graph, so conditional includes (e.g., inside `if`) only appear in the chain when they were actually evaluated.

## Document Pipeline and $include — Formal Specification

This section formalizes the inter-file include mechanism. The intra-file document pipeline (`%` threading via `---` boundaries) and intra-document scope chains are already formalized in §Scope Chain Semantics — Formal Specification (DOC-PIPELINE and SEQ-SCOPE rules, respectively). This section covers `$include`: path resolution, cycle detection, result caching, and the eager materialization invariant.

### Part 1: Include State

The include system maintains mutable state `Σ` shared across nested include calls:

```
Σ = ⟨base_dir, guard, cache, stdlib_env⟩  where
  base_dir   : Path              — directory of the currently-evaluating file
  guard      : Set<Path>         — canonical paths currently being evaluated (cycle detection)
  cache      : Map<Path, Rc<Thunk>>  — canonical path → evaluated result (memoization)
  stdlib_env : ρ                 — environment for included files (builtins + stdlib)
```

`Σ` is stored in a thread-local (`INCLUDE_CTX`). All mutations are scoped: `guard` entries are pushed before recursion and popped after (even on error); `base_dir` is saved and restored around each include. `cache` entries are append-only — once a file is cached, its result is never replaced.

**Threading model:** `Σ` is threaded via `Rc<RefCell<EvalContext>>` — the `EvalContext` parameter passed through all evaluation functions. The formal semantics are independent of the threading mechanism — `Σ` transitions are the same regardless of how `Σ` is carried.

### Part 2: Path Resolution

**[RESOLVE]** — Path resolution and canonicalization:

```
resolve(path_str, Σ.base_dir):
  raw = Path::new(path_str)
  resolved = if raw.is_absolute() then raw
             else Σ.base_dir / raw
  canonical = canonicalize(resolved)       (resolves symlinks, normalizes ..)
  ────────────────────────────────────────
  ⇒ canonical : Path
```

Canonicalization serves two purposes: (1) cycle detection requires path identity — `./lib/../lib/utils.llt` and `lib/utils.llt` must resolve to the same key; (2) caching requires the same identity guarantee. Canonicalization fails with an I/O error if the path does not exist on the filesystem.

**Allowlist check:** An INCLUDE-DENY rule is inserted between RESOLVE and INCLUDE-HIT, rejecting paths outside allowed directories before consulting the cache. The check ordering is: canonicalize → allowlist → cache → cycle → read.

### Part 3: Include Rules

Three rules cover the three possible outcomes of an include call. They are checked in priority order: cache → cycle → evaluate. A fourth outcome — INCLUDE-DENY — precedes all three when the path falls outside the allowed directories.

In all rules below, `d` is the evaluation depth and `s` is the call-site span (used for error reporting but not for rule selection).

**[INCLUDE-HIT]** — Cache hit (memoized result):

```
resolve(path_str, Σ.base_dir) ⇒ canonical
canonical ∈ dom(Σ.cache)
────────────────────────────────────────
include(path_str, Σ, d, s) ⇒ Ok(Rc::clone(Σ.cache[canonical]))
```

Cache hits return a clone of the cached thunk pointer. No file I/O, no evaluation. This is Jsonnet-style import memoization: multiple includes of the same file share a single evaluation result.

**Cache implementation details:**

- **Cache key:** Canonical `PathBuf` (after symlink resolution and normalization via `std::fs::canonicalize`). Different relative paths that resolve to the same file share a single cache entry — `./lib/utils.llt` and `subdir/../lib/utils.llt` hit the same cache key if they canonicalize to the same absolute path.

- **Cache scope:** Thread-local, stored in `EvalContext::state::include_cache` (`eval.rs:43`, `HashMap<PathBuf, Rc<Thunk>>`). Each thread has its own cache; no cross-thread sharing. The cache is shared across all nested `$include` calls within a single evaluation session.

- **Cache lifetime:** Lives as long as the `EvalContext`. In the CLI, a single `EvalContext` is created per `tinct eval` invocation and cleared on exit. In the REPL, the `EvalContext` persists across REPL inputs, so included files are cached for the entire REPL session — a file modified on disk mid-session will not be re-read until the REPL is restarted.

- **Error non-caching:** Failed includes are NOT cached. If `$include("broken.llt")` fails (parse error, I/O error, eval error), subsequent `$include("broken.llt")` calls re-attempt evaluation. Only successful results populate the cache. Note that the call-site thunk caches the failure (via `ThunkState::Failed`) — the same call site will not retry — but a different call site including the same file will retry the file-level evaluation.

**[INCLUDE-CYCLE]** — Cycle detection:

```
resolve(path_str, Σ.base_dir) ⇒ canonical
canonical ∉ dom(Σ.cache)
canonical ∈ Σ.guard
────────────────────────────────────────
include(path_str, Σ, d, s) ⇒ Err("circular include detected: {canonical}")
```

A file currently being evaluated (present in the guard set) cannot be included again. This catches direct cycles (`A includes A`) and transitive cycles (`A includes B includes A`). The error is raised at the include call site — no evaluation of the cyclic file is attempted.

**[INCLUDE-EVAL]** — Fresh evaluation:

```
resolve(path_str, Σ.base_dir) ⇒ canonical
canonical ∉ dom(Σ.cache)
canonical ∉ Σ.guard
assert file_size(canonical) ≤ MAX_FILE_SIZE             (10 MB; prevents resource exhaustion)
source = read_file(canonical)                           (I/O: file read)
file = parse(source)                                    (parse tinct source)
desugar(file)                                           (AST transformation: $_ implicit lambdas)

Σ.guard ← Σ.guard ∪ {canonical}                        (push guard)
saved_base = Σ.base_dir
Σ.base_dir ← parent(canonical)                         (set base_dir for nested includes)

θ = eval_file(file, Σ.stdlib_env, d + 1)               (evaluate all documents)
v = materialize(θ, None, d + 1)                         (EAGER materialization — see Part 4)

Σ.base_dir ← saved_base                                (restore base_dir)
Σ.guard ← Σ.guard \ {canonical}                        (pop guard)

θ_result = Materialized(v)                              (pure allocation — no evaluation)
Σ.cache[canonical] ← θ_result                          (cache result)
────────────────────────────────────────
include(path_str, Σ, d, s) ⇒ Ok(θ_result)
```

On error at any step (file read, parse, eval, materialize), the `base_dir` and `guard` are restored before the error propagates — the INCLUDE-RESTORE invariant (Property 3 below).

The `d + 1` depth propagation means nested includes consume evaluation depth. Deep include chains eventually hit `MAX_EVAL_DEPTH`, providing an independent bound on include recursion beyond the guard set.

The included file evaluates with `Σ.stdlib_env` as its root scope and `%` initialized to the empty dict (`eval_file` passes `None` as `initial_input` to `eval_file_with_input`, which defaults to `Materialized(Dict([]))`). It does *not* receive the including file's scope chain — include isolation is strict (Property 5).

### Part 4: Eager Materialization Invariant

`$include` is one of three builtins that eagerly materialize their result (the others are `$eval` and `$try`). `$include` uses single-level `materialize` (forces the outer dict but leaves nested values as thunks), while `$eval` uses `deep_materialize` (recursively forces all nested thunks with cycle detection). `$try` materializes the function body result to determine success or failure. The eager materialization in INCLUDE-EVAL is required for correctness of the guard-based cycle detection:

**Why not lazy?** If `$include` returned `θ` (the unevaluated result thunk) instead of `Materialized(v)`:

1. **Cycle detection breaks.** The guard entry for `canonical` is popped immediately after `eval_file` returns. A lazy result defers actual evaluation of nested `$include` calls within the result — when those deferred thunks are later forced, `canonical` is no longer in the guard set, so a transitive cycle would go undetected.

2. **Path resolution breaks.** The `base_dir` is restored to the parent file's directory after the include returns. If the included file's result contains nested `$include` calls (as unevaluated thunks), those calls would resolve relative paths against the *parent's* `base_dir`, not the included file's directory.

3. **Cache coherence breaks.** The cached result must be a fully evaluated value so that all consumers receive semantically equivalent data. A lazy cached thunk could produce different results depending on evaluation context (depth, base_dir at the time of forcing).

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

**Previously known defect (resolved):** Earlier versions violated P3 for materialization errors — the `materialize` call used the `?` operator, which returned before cleanup ran. This has been fixed by using an explicit `match` with cleanup in both branches.

**P4 — Include determinism (conditional):** For a fixed filesystem state, the document pipeline `eval_file(file, ρ, d)` is deterministic. When the filesystem changes between evaluations, results may differ — `$include` is the sole source of nondeterminism in tinct (see §Thunk Lifecycle — Semantic Properties, Determinism; also Semantic Commitment 2 in §Thunk Lifecycle — Semantic Commitments).

**P5 — Include isolation:** An included file has no access to the including file's scope chain. Included files evaluate in `Σ.stdlib_env` (builtins + stdlib only), with `%` initialized to the empty dict:

```
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
| Σ (EvalState) | `eval.rs:41-45` (`include_guard`, `include_cache`) |
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
| DOC-PIPELINE (cross-ref) | `eval_file_with_input` (`eval.rs:820-859`) |
| SEQ-SCOPE (cross-ref) | `eval_document` (`eval.rs:199-249`) |

## Pure Language, CLI Handles I/O

Tinct is a pure data transformation language with no in-language side effects, modulo `$include`, which performs filesystem I/O as a controlled side effect with sandboxing (similar to Nix's `import` and Dhall's `import`). The program evaluates to a value; the CLI serializes it:

```
tinct eval file.llt              # evaluate, output result as JSON
tinct eval --eval file.llt       # deep-force all thunks before serializing (surfaces errors before partial output)
tinct eval -                     # read Tinct source from stdin
cat data.json | tinct eval file.llt  # stdin JSON parsed and injected as % for first document
```

This is the Jsonnet/Nix model: the language produces data, an external tool handles I/O. Unreferenced dict entries are never computed. There is no `$write`, `$read`, `$stdout`, `$stdin`, or channel system.

`$eval` is a runtime-supported function that recursively forces all thunks in its argument. It performs full materialization: the entire structure is forced into memory. The implementation caps recursion at depth 256 and returns an error if exceeded. On infinite or cyclic structures, `$eval` will hit the depth limit rather than diverging. Use `$take` to bound infinite sequences before passing them to `$eval`.

```tinct
# Without eval: CLI serializes lazily (streaming, may partially output then hit an error)
[result: [map %.data [fn [x] [+ x 1]]]]

# With eval: everything forced into memory first (errors caught before any output)
[result: [eval [map %.data [fn [x] [+ x 1]]]]]

# Safe on infinite sequences: take bounds before eval
[result: [eval [take 100 %.sequence]]]
```

**Why pure?** In-language I/O in a lazy language creates a forcing problem: side-effecting expressions buried in lazy dict entries may never execute, and execution order becomes unpredictable. By making the language pure, lazy evaluation is semantically transparent — the result is the same regardless of evaluation order. The CLI is the only I/O boundary, and it forces exactly what it needs to serialize the output.

**Security:** External input (stdin, files) is parsed by the CLI and injected as structured data (`%`). The language never evaluates untrusted input as code. `$from-json` is a pure function that converts a JSON string to a dict — safe on untrusted input.
