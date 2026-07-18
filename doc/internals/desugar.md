# Desugar

This document is for Rust contributors working in `src/desugar.rs`. Tinct developers should be aware of one key consequence: `[fn [let _] ...]` lambdas, pipe rewrites, and `tmpl`/`unindent` calls appear in the AST from this pass onward — any downstream pass sees those forms, not the original `$_`, `|`, or `i"..."` surface syntax.

Desugaring is a source-to-source transformation on the `SurfaceProgram` that runs after parsing and before name resolution. It currently performs three independent rewrites in two passes. Both passes mutate the `SurfaceProgram` in place using `Arc::make_mut` or `Arc::get_mut`. After desugaring completes, the AST contains no `Pipe` nodes, no `$_` placeholders, no `i"..."` interpolated string nodes, and no triple-quoted `"""..."""` string nodes — all have been rewritten into ordinary `Call`, `Fn`, and `StringLiteral` nodes.

---

## Pass Order

The two passes must run in this order:

```
1. desugar_instance_decls_surface_program()   — InstanceDecl → Dict
2. desugar_surface_program()                  — $_ → Fn, Pipe → Call, strings → prelude calls
```

Pass 1 runs first because pass 2 recurses into dict entries. If an `InstanceDecl` is not already expanded into a `Dict`, pass 2 skips the method bodies inside it and they remain untransformed.

**Call-site requirement:** Callers must invoke both passes in this order. Calling only `desugar_surface_program` skips the InstanceDecl expansion. Calling them in reverse order leaves InstanceDecl method bodies undesugared. `builtin-desugar` (`builtins_meta.rs`) is the only production caller that correctly runs both passes in the right order for user-initiated pipeline calls. The CLI paths in `main.rs`, `lib.rs`, `imports.rs`, and `formatter.rs` each use their own call sequences — see [Callers](#callers).

---

## Mutation Model

Pass 1 (`desugar_instance_decls_surface_program`) uses `Arc::get_mut()` to obtain exclusive write access to each `Arc<SurfaceDocument>`. This panics if any other `Arc` reference to the document exists:

```rust
Arc::get_mut(&mut doc_spanned.node).expect("desugar runs before any Arc sharing")
```

Pass 2 (`desugar_surface_program`) uses the same `Arc::get_mut()` pattern at the document level, then `Arc::make_mut()` when recursing into individual `Arc<SurfaceNode>` values inside each document.

Both patterns enforce that desugaring runs before any sharing of AST nodes — no clone-on-write contention, no concurrent access. After both passes complete, other subsystems may clone or share `Arc` nodes freely.

The `builtin-desugar` builtin (`builtins_meta.rs:2874`) explicitly checks that all document Arcs have strong count 1 before calling the passes and returns an `EvalError` if not, since calling `builtin-program-docs` first would share the document Arcs and cause a panic.

---

## Pass 1 — `desugar_instance_decls_surface_program`

**What it does:** Converts single-arm `InstanceDecl` entries in dict-entry position into plain `Dict` nodes containing the instance methods.

**Why:** Named single-arm instances like `MonadResult: [instance Monad [let m@Result]: [bind: ...]]` need runtime method access (`MonadResult.bind`). The evaluator handles `Dict` values naturally; an `InstanceDecl` node requires special-casing in every consumer. Expanding single-arm instances here eliminates that special-casing.

**Single-arm only:** Only instances with exactly one arm are expanded. Multi-arm instances (e.g., `[instance Addable [let a@String b@String c]: [...] [let a@Int b@Int c]: [...]]`) are left as `InstanceDecl` so `lower.rs` can emit all arms as `instance_binding_name`-keyed dict entries. Expanding only `arms[0]` of a multi-arm instance would silently discard all other arms (this was bug B-409).

**Example:**
```
Before: MonadResult: [instance Monad [let m@Result]: [bind: [fn ...]]]
After:  MonadResult: [bind: [fn ...]]
```

The transformation recurses into nested dicts, calls, fn bodies, match arms, and sequential expressions to find all embedded `InstanceDecl` entries. It uses a copy-on-write pattern via `Arc::ptr_eq` comparison: if no recursion produced a changed node, the original `Arc` is returned without allocating.

---

## Pass 2 — `desugar_surface_program`

This pass performs three independent rewrites in a single tree walk: `$_` desugaring, `Pipe` lowering, and string literal desugaring.

### Rewrite 1: `$_` → Implicit Lambda

`$_` (the underscore placeholder, written as `_` in call position after `$`) desugars into an explicit lambda. The rules are:

**DIRECT rule** — when `$_` appears as a direct positional argument to a call:
```
[f a _ b]  →  [fn [let _] [f a _ b]]
```

**WRAP rule** — when `$_` appears nested inside a sub-expression:
```
[f [g _] b]  →  [fn [let _] [f [g _] b]]
```

In both cases, the entire enclosing call (not just the sub-expression) is wrapped in a lambda. If multiple `$_` uses appear in the same call, all refer to the same single parameter.

**WRAP-DOT:** An access chain rooted at `$_` (e.g., `$_.name`) is also treated as DIRECT, wrapping the dot expression in a lambda: `$_.name → [fn [_] $_.name]`.

**WRAP-PIPE:** `$_ | f` is also wrapped: `$_ | f → [fn [_] [$_ | f]]`. The pipe is lowered after wrapping.

**WRAP-DICT:** `$_` as a dict value (not key) triggers wrapping: `[k: _]` → `[fn [_] [k: _]]`.

**Shadowing:** When a `Fn` node has `_` as an explicit parameter name, `depth` is incremented for the body recursion. At `depth > 0`, the `$_` is already bound and no wrapping occurs.

**Generated fn markers:** The synthesised lambda node has `desugared: true` in its `SurfaceExpression::Fn` so downstream passes can distinguish user-written `[fn [let _] ...]` from synthesised ones.

**Depth invariant:** The lambda wrapper adds one level of nesting. Since `$_` can only appear inside an expression that the parser already accepted (depth ≤ 256), desugaring cannot produce ASTs deeper than the parse depth limit.

**After desugar:** The type checker and evaluator see only `Fn` nodes and `Var("_")` references. No `$_` placeholder nodes reach them.

### Rewrite 2: `Pipe(lhs, rhs)` → `Call`

All `Pipe` nodes are rewritten to `Call` nodes:

```
Pipe(lhs, Call(f, args))  →  Call(f, args ++ [lhs])   — lhs appended as last positional arg
Pipe(lhs, VarRef(name))   →  Call(VarRef(name), [lhs]) — bare name applied to lhs
Pipe(lhs, other)          →  Call(other, [lhs])         — any other rhs called with lhs
```

The LHS is always appended as the **last** positional argument. This is what makes `subject | [f a b]` equivalent to `[f a b subject]`, following the subject-last convention.

**Chain flattening:** The parser produces right-associative trees: `a | b | c | d` parses as `Pipe(a, Pipe(b, Pipe(c, d)))`. A naïve recurse-then-rewrite approach would mis-nest the result. The correct approach (implemented in `desugar_pipe_chain`) is:

1. Flatten the right-associative chain into a stage list: `[a, b, c, d]`
2. Desugar each stage independently (not as a pipe sub-chain)
3. Left-fold with `apply_pipe_step`: `acc = a; acc = [b acc]; acc = [c acc]; acc = [d acc]`
4. Result: `[d [c [b a]]]` (correct left-associative nesting)

**Defensive fallback in `lower.rs`:** `lower.rs` contains a `Pipe → Call` arm (line 511) that fires if a `Pipe` node somehow survives to lowering. This should never happen in practice — it exists as a safety net, not as a planned code path. The type checker's `extract_doc_from_surface_node` (called from `typecheck.rs`) also handles `Pipe` nodes by recursing into both sides, which is similarly defensive. `typecheck_cek.rs` handles `Pipe` in its free-variable walk for the same reason.

### Rewrite 3: String Literal Desugaring

`StringLiteral` nodes are rewritten based on their `prefix` and `delimiter` fields:

| prefix | delimiter length | Transformation |
|--------|-----------------|----------------|
| `"i"` | 1 (single `"`) | `[tmpl "template"]` call |
| `""` | ≥ 3 (triple `"""`) | `[unindent [StringLiteral ...]]` call |
| `"i"` | ≥ 3 (triple `"""`) | `[unindent [tmpl "template"]]` call |
| `""` | 1 | unchanged (plain string literal — lowering handles escape sequences) |

**Interpolated string processing (`i"..."`):** The desugar pass scans the raw content character by character:
- `$$` → kept as `$$` in the template string (interpreted by `tmpl` as a literal `$`)
- `$ident` → kept as `$ident` in the template string (`tmpl` resolves variable names)
- `$` followed by anything else → passed through as a literal `$`

The result is a single `[tmpl "template-string"]` call. Variable names in the template are resolved by the prelude `tmpl` macro at eval time, not by the desugar pass. The `${expr}` form is not supported — only `$ident` variable references exist in tinct string literals.

**Triple-quoted string processing (`"""..."""`):** The string is wrapped in `[unindent ...]`. The inner `StringLiteral` node retains the triple-quote delimiter so that `lower.rs` can distinguish it from single-quoted strings and skip escape-sequence processing (triple-quoted strings pass content raw; `\n`, `\t`, etc. are literal backslash sequences in triple-quoted strings, not escape sequences).

**Protocol dependency:** `tmpl` and `unindent` are names that must be defined in any prelude that supports interpolated and triple-quoted strings. They are not Rust builtins — they are protocol requirements that the desugar pass unconditionally references. An open decision (tracked as D-3) covers whether these should become Rust builtins or remain prelude-defined.

**Quote boundary:** `$_` and string literals inside a `Quote` node are **not** desugared. The quoted AST is frozen. `Unquote` and `UnquoteSplice` nodes inside a `Quote` are recursed into normally.

---

## Transformations NOT in This Pass

The following are sometimes assumed to be desugaring but are not:

- **`begin` / `>>`:** Not desugared. `>>` is a prelude macro that takes `...body@Expr` variadics and desugars at eval time via quasiquoting. `begin` is an alias for `>>` in the prelude. The `Sequential` AST node (produced by the parser for multi-expression fn bodies) is a distinct concept — it represents letrec-scoped intermediate dicts in fn/match bodies, not `>>`.
- **`->` (arrow pipe):** Not desugared. `->` is a prelude function (`thread`/`reduce`) that accepts a list of functions and applies them left-to-right.
- **`@` type assertions:** Not desugared. `TypeAssert` nodes are preserved and processed by the type checker and lowering pass.
- **Leading-dot field access (`.field`):** Not desugared. Passed through to the resolver which rewrites it to a parent-scope lookup.

---

## Entry Points

```rust
/// Pass 1: InstanceDecl → Dict. Must run before pass 2.
pub fn desugar_instance_decls_surface_program(program: &mut SurfaceProgram)
```

```rust
/// Pass 2: $_ → Fn, Pipe → Call, strings → prelude calls. Mutates in place.
pub fn desugar_surface_program(program: &mut SurfaceProgram)
```

```rust
/// Desugar a single standalone Arc<SurfaceNode> at the given lexical depth.
/// Used by REPL input and eval.rs test helpers that hold a standalone node.
/// depth = 0 for top-level callers; depth > 0 suppresses wrapping inside existing lambdas.
pub fn desugar_surface_node(node: &mut Arc<SurfaceNode>, depth: usize)
```

`desugar_surface_node` is the public single-node entry point that wraps the private recursive implementation. It only applies pass 2 transformations (no InstanceDecl expansion). Callers that need InstanceDecl expansion must call `desugar_instance_decls_surface_program` on the containing program before using this function.

---

## Callers

The two desugar passes are called across multiple sites. Not all sites call both passes.

| Caller | Pass 1 | Pass 2 | Notes |
|--------|--------|--------|-------|
| `builtins_meta.rs` `builtin-desugar` | yes (first) | yes (second) | Correct order. The canonical thin-builtin pipeline entry point. |
| `main.rs` `run_literate_check` | yes (second) | yes (first) | **Reversed order** — `desugar_surface_program` is called before `desugar_instance_decls_surface_program`. Instance method bodies inside single-arm instances may not be fully desugared. |
| `main.rs` `run_eval`, `run_lint`, `run_describe` | no | yes | InstanceDecl expansion skipped. |
| `lib.rs` `run_loader`, `typecheck_source`, `typecheck_source_errors_only`, tests | no | yes | InstanceDecl expansion skipped. |
| `imports.rs` `build_builtin_core_type_env_inner` | no | yes | InstanceDecl expansion skipped. |
| `formatter.rs` | no | yes | InstanceDecl expansion skipped. |
| `builtins.rs` test helper `parse_eval` | no | yes | Test-only; no instances expected. |
| `eval.rs` test helpers | no | yes (via `desugar_surface_node`) | Test-only. |
| `resolve.rs` test helper `parse_and_resolve` | no | yes | Test-only; no instances expected. |

The widespread omission of pass 1 is only safe because `InstanceDecl` nodes in dict-entry position are rare in most code paths — they primarily appear in user-defined typeclass instances. However, the asymmetry between callers is a latent correctness risk: any path that processes user code with named single-arm instances and skips pass 1 will leave `InstanceDecl` nodes in the AST that downstream passes are not prepared to handle.

---

## Interaction with Other Subsystems

### What desugar receives (from the parser)

The parser produces `SurfaceProgram` with:
- `Pipe` nodes for `|` operators
- `VarRef { name: "_" }` for `$_` placeholders  
- `StringLiteral { prefix: "i", .. }` for `i"..."` strings
- `StringLiteral { delimiter: "\"\"\"", .. }` for `"""..."""` strings
- `InstanceDecl` wrapped in `Decl(...)` for `[instance ...]` forms in named-entry position

### What desugar produces (for the resolver)

After both passes, the program contains only:
- `Call` nodes (no `Pipe`)
- `Fn` nodes (no raw `$_` references — the `_` name may appear as a bound parameter)
- Ordinary `StringLiteral { prefix: "", delimiter: "\"" }` nodes or `Call` nodes to `tmpl`/`unindent`
- `Dict` nodes for expanded single-arm instances; `InstanceDecl` for unexpanded multi-arm instances

### Resolver

The resolver (`resolve.rs`) runs after desugaring and writes De Bruijn coordinates inline into `VarRef.resolution` and `Field.resolution` fields. It must see `Fn` nodes where `$_` was used — if desugaring ran after resolution, the `_` parameter in the generated `Fn` would have no resolution and all references to `_` inside the body would be unresolved.

The resolver handles `Pipe` nodes defensively (line 257, 903) — these arms exist because the resolver is used in a few paths (e.g., `resolve.rs` test helper) that do not guarantee desugar ran first.

### Type Checker

The type checker (`typecheck.rs`, `typecheck_cek.rs`) asserts `Pipe` is unreachable via `todo!()` in the main type-inference path (`typecheck_cek.rs:3304` handles it by recursing both sides in the free-variable walk, which is a diagnostic utility, not the main inference path). The main type-inference pass (`typecheck_cek.rs`) cannot type-check a `Pipe` node — it is not a supported expression form. If desugaring is skipped, pipe expressions silently fail to type-check.

### Lowering (`lower.rs`)

The lowering pass converts `SurfaceExpression` → `CoreExpr`. It contains a defensive `Pipe → Call` arm (line 511) that handles any `Pipe` nodes that survive to this stage. In a correctly-ordered pipeline this arm never fires — it is a safety net.

### Evaluator

The evaluator (`eval.rs`, `eval_core.rs`) does not directly handle `Pipe` nodes. `eval_core.rs` contains a `Pipe` arm only inside `eval_quote_preprocess`, which reconstructs the surface AST during quasiquote evaluation — a specialized code path, not the main evaluator. The main evaluator operates on `CoreExpr`, which has no `Pipe` variant.

---

## Invariants

1. **Runs before name resolution.** The resolver must see `Fn` nodes where `$_` was used. Running desugar after the resolver produces unresolved `_` references.
2. **Runs before typechecking.** The main type inference path cannot handle `Pipe` nodes. String literal desugaring must also run before typechecking so the type checker sees `tmpl`/`unindent` calls, not raw `i"..."` nodes.
3. **Pass 1 before pass 2.** `desugar_instance_decls_surface_program` must run before `desugar_surface_program` so that InstanceDecl method bodies are visible to the `$_` and pipe rewrites.
4. **`Arc::get_mut()` exclusivity.** Both passes panic if any other `Arc` reference to the document exists. Desugaring must run before any operation that clones or shares `Arc<SurfaceDocument>` (e.g., `builtin-program-docs`).
5. **No `Pipe` nodes after pass 2.** No downstream pass should produce or re-introduce `Pipe` nodes. The defensive fallbacks in `lower.rs` and the resolver exist for robustness, not as planned code paths.
6. **Quote boundary.** `$_` and string literals inside `Quote` nodes are not desugared. `Unquote`/`UnquoteSplice` nodes inside a `Quote` are recursed into normally.
7. **`tmpl` and `unindent` must be defined by the prelude.** Any prelude omitting these names will fail to evaluate interpolated or triple-quoted strings. This is a protocol constraint between the desugar pass and the prelude.
