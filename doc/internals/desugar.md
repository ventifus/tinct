# Desugar

Desugaring is a two-pass source-to-source transformation on the `SurfaceProgram` that runs after parsing and before name resolution. Both passes mutate the `SurfaceProgram` in place. After desugaring, the AST contains no `Pipe` nodes and no `$_` placeholders.

---

## Pass Order

The two passes must run in this order:

```
1. desugar_instance_decls_surface_program()   — InstanceDecl → Dict
2. desugar_surface_program()                  — $_ → Fn, Pipe → Call
```

Pass 1 runs first because pass 2 recurses into dict entries; if an `InstanceDecl` is not already expanded into a `Dict`, pass 2 would skip the method bodies inside it.

---

## Mutation Model

Both passes use `Arc::get_mut()` to obtain exclusive access to each `Arc<SurfaceDocument>` before mutating it. This panics if any other `Arc` reference to the document exists:

```rust
Arc::get_mut(&mut doc_spanned.node).unwrap_or_else(|| {
    panic!("document Arc has {count} strong references, expected 1")
})
```

This enforces that desugaring runs before any sharing of AST nodes — no clone-on-write, no concurrent access. After desugaring completes, other passes may share the `Arc` nodes freely.

---

## Pass 1 — `desugar_instance_decls_surface_program`

**What it does:** Converts single-arm `InstanceDecl` entries in dict-entry position into plain `Dict` nodes containing the instance methods.

**Why:** Named single-arm instances like `MonadResult: [instance Monad [let m@Result]: [bind: ...]]` need runtime method access (`MonadResult.bind`). The evaluator handles `Dict` values naturally; an `InstanceDecl` node requires special-casing in every consumer. Expanding single-arm instances here eliminates that special-casing.

**Single-arm only:** Only instances with exactly one arm are expanded. Multi-arm instances (e.g., `[instance Addable [let a@String b@String c]: [...] [let a@Int b@Int c]: [...]]`) are left as `InstanceDecl` so `lower.rs` can emit all arms as `instance_binding_name`-keyed dict entries. Expanding only `arms[0]` of a multi-arm instance would silently discard all other arms.

**Example:**
```
Before: MonadResult: [instance Monad [let m@Result]: [bind: [fn ...]]]
After:  MonadResult: [bind: [fn ...]]
```

The transformation recurses into nested dicts, calls, fn bodies, match arms, and sequential expressions to find all embedded `InstanceDecl` entries.

---

## Pass 2 — `desugar_surface_program`

This pass performs two independent rewrites: `$_` desugaring and `Pipe` lowering.

### `$_` → Implicit Lambda

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

**Depth invariant:** The lambda wrapper adds one level of nesting. Since `$_` can only appear inside an expression that the parser already accepted (depth ≤ 256), desugaring cannot produce ASTs deeper than the parse depth limit.

**After desugar:** The type checker and evaluator see only `Fn` nodes and `Var("_")` references. No `$_` placeholder nodes reach them.

### `Pipe(lhs, rhs)` → `Call`

All `Pipe` nodes are rewritten to `Call` nodes:

```
Pipe(lhs, Call(f, args))  →  Call(f, args ++ [lhs])   — lhs appended as last positional arg
Pipe(lhs, VarRef(name))   →  Call(VarRef(name), [lhs])
Pipe(lhs, other)          →  Call(other, [lhs])
```

The LHS is always appended as the **last** positional argument. This is what makes `subject | [f a b]` equivalent to `[f a b subject]` — it follows the subject-last convention where the data being operated on is the last parameter, enabling natural pipeline composition.

After this pass:
- The type checker has `unreachable!()` for any `Pipe` node it encounters.
- `lower.rs` has a defensive `Pipe → Call` arm but it should never fire in practice.

---

## Entry Points

```rust
pub fn desugar_instance_decls_surface_program(program: &mut SurfaceProgram)
```

Pass 1. Must run before pass 2.

```rust
pub fn desugar_surface_program(program: &mut SurfaceProgram)
```

Pass 2. Runs `$_` desugaring and pipe lowering together in one tree walk.

---

## Invariants

1. **Runs before name resolution.** The resolver must see `Fn` nodes where `$_` was used, not `VarRef("_")` nodes in call position. Running desugar after the resolver would produce unresolved `_` references.
2. **Runs before typechecking.** The type checker asserts `Pipe` is unreachable; it must not see any `Pipe` nodes.
3. **Single-arm only for InstanceDecl expansion.** Multi-arm instances are always left intact for `lower.rs`.
4. **`Arc::get_mut()` exclusivity.** Both passes panic if any other `Arc` reference to the document exists. Desugaring must run before AST sharing.
5. **No `Pipe` nodes after pass 2.** Callers may assert this; no downstream pass produces or handles `Pipe`.
