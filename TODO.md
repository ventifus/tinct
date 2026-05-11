# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Research (requires /rnd before implementing)

- Constraint annotation syntax — should users be able to write explicit type class constraints in annotations (e.g. `@[Equatable a]`)? Currently constraints are implicit and inferred. Motivating use cases: LSP hover copy-paste principle (constraint info is advisory-only today); stdlib authors wanting to explicitly declare polymorphic interface requirements. Write design note in `doc/whatif/` first.

---

## Type System Cleanup

### builtin-type-audit: Fix Unknown→Any/Never in builtin type registrations

Audit and fix incorrect `Type::Unknown` uses in `TypeEnv::with_builtins()` (`src/type_env.rs`).
`Unknown` = gradual-typing opt-out (consistency, not subtyping); `Any` = accepts anything within the lattice; `Never` = does not return.

- [ ] `length`: remove stale `TODO(length-narrow-type)` comment and stale `RowTail::RowVar` reference; update registration to `Union(Dict, String, Bytes)` → `Int` since `length-narrow-type` sprint is already complete (`src/type_env.rs`)
- [ ] `if` return: `Unknown` → `Any` for both branch params and return type (`src/type_env.rs`)
- [ ] `append` value param: second param `Unknown` → `Any` — it accepts any value but is not a type-checking opt-out (`src/type_env.rs`)
- [ ] `apply` return: `Unknown` → `Any` (`src/type_env.rs`)
- [ ] `try` return: `Unknown` → `Any` (`src/type_env.rs`)
- [ ] `force`: `(Unknown) → Unknown` — change to pass-through TypeVar or `(Any) → Any` (`src/type_env.rs`)
- [ ] `error` return: `Unknown` → `Never` — `error` always throws, never returns a value (`src/type_env.rs`)
- [ ] `slurp` return: `Unknown` → `String` — reads file contents as a string (`src/type_env.rs`)
- [ ] `env` return: `Unknown` → `String` — reads environment variable as a string (`src/type_env.rs`)
- [ ] Add param names to `with_builtins()` registrations for common builtins (aids LSP hover): `set`, `get`, `has?`, `append`, `merge`, `if`, `map`, `filter`, `reduce` at minimum

---

## Higher-Kinded Types

Accepted 2026-05-11. See `doc/whatif/hkt-monads.md` for the full design.
Adds `Kind::Operator` (`* → *`), `Type::App`, `Type::Operator(String)`, the Functor/Applicative/Monad/Foldable/Mappable/Appendable typeclass hierarchy, Maybe ADT, generic functions (sequence, traverse, forM, when, liftM2), and inferred `[do]`.

### hkt-foundation: Kind system, App/Operator types, annotation parsing

See `doc/whatif/hkt-monads.md` §Syntax Design, §Formal Type Rules, §What Would Change. **Spec chapters:** `doc/whatif/hkt-monads.md §Syntax Design`, `§Formal Type Rules`.

- [ ] Add `Kind::Operator` variant to `Kind` enum alongside existing `Kind::Type`/`Kind::Row`/`Kind::Var` (`src/types.rs`)
- [ ] Add `Type::App(Box<Type>, Box<Type>)` variant for type constructor application (`src/types.rs`)
- [ ] Add `Type::Operator(String)` variant for type constructor variables — note: `Type::Operator` is distinct from `Kind::Operator`; the former is a type-level term, the latter a kind-level classifier (`src/types.rs`)
- [ ] Update all exhaustive match sites for new `Type` variants: `src/desugar.rs`, `src/formatter.rs`, `src/eval.rs`, `src/typecheck.rs`, `src/lsp/analysis.rs`, `src/ast_dict.rs`, `src/type_unify.rs` — add `App`/`Operator` arms everywhere `Type` is matched
- [ ] Add `Display` impls for `Type::App` (`"[{f} {a}]"`) and `Type::Operator` (`"{name}"`) in `src/type_env.rs`
- [ ] Add `UNIFY-OPERATOR` rule to `src/type_unify.rs`: `unify(Operator(m), T) = [m ↦ T]` with occurs check; `unify(T, Operator(m)) = [m ↦ T]` symmetric
- [ ] Add `UNIFY-APP` rule to `src/type_unify.rs`: decompose `App(f₁, a₁)` against `App(f₂, a₂)` by unifying constructors then arguments
- [ ] Recognize `Operator` as a reserved kind-level name in annotation parsing (`src/typecheck_annot.rs`) — when annotation text is exactly `"Operator"`, produce `Kind::Operator` not a type lookup
- [ ] In annotation positions, parse `[f a]` (no colons) as `Expr::TypeApp(f, a)` when `f` resolves to an Operator-kinded type variable or user parameterized type alias; builtins (`Seq`, `Map`) keep existing `@Seq@T` path (`src/parser.rs`, `src/typecheck_annot.rs`)
- [ ] Extend `class` declaration parsing to accept `extends [SuperClass param]` clause — parser emits `ClassDecl { superclasses: Vec<(String, String)> }` (`src/parser.rs`, `src/ast.rs`)
- [ ] Tests: corpus tests for `@Operator` kind annotation, `@[m a]` type application, `extends` syntax, `Type::App` Display, `UNIFY-APP` and `UNIFY-OPERATOR` unit tests (`src/type_unify.rs`, `tests/corpus/eval/typecheck/`)

### hkt-kind-inference: Kind checking pass and Operator-kinded class resolution

See `doc/whatif/hkt-monads.md` §Formal Type Rules §Kind Checking, §Typeclass Resolution for HKT. **Spec chapters:** `doc/whatif/hkt-monads.md §Formal Type Rules`.

- [ ] Add kind inference pre-pass in `src/typecheck.rs` before HM inference: walk class method signatures, assign `Kind::Operator` to parameters annotated `@Operator` or constrained by an Operator-kinded class (`Monad`, `Functor`, etc.)
- [ ] Implement `KIND-OPERATOR` rule: `Γ ⊢ f : Operator  Γ ⊢ a : *  ⟹  Γ ⊢ App(f, a) : *` — validate kind-correctness of `App` types during annotation resolution (`src/typecheck.rs`, `src/typecheck_annot.rs`)
- [ ] Enforce rank-1 restriction: reject `App(Operator("f"), Operator("g"))` where both `f` and `g` are unbound Operator variables — emit kind error at the annotation site (`src/typecheck.rs`)
- [ ] Extend `ClassEnv` lookup in `src/type_env.rs` to handle Operator-kinded class parameters: when resolving a constraint `C m` where `m : Operator`, match against instance entries using `UNIFY-APP` on the instance head
- [ ] Add `App` type inference in `src/typecheck.rs`: when a binding has inferred type `App(Operator("m"), a)`, unify `m` against known instance heads via `UNIFY-OPERATOR` to resolve the concrete type constructor
- [ ] Normalize `App(Seq, T)` → `Type::Seq(T)` and `App(Map, K, V)` → `Type::Map(K, V)` when an Operator variable resolves to a builtin type constructor — normalization happens in `src/typecheck_annot.rs` at instance resolution time, not at parse time
- [ ] Kind error messages: when a kind-`*` type is used where `Operator` is expected (or vice versa), emit a `TypeError` with message "kind mismatch: expected type constructor (Operator), got concrete type (*)" at the annotation span (`src/typecheck.rs`)
- [ ] Tests: corpus tests for kind mismatch errors, App type inference, rank-1 violation rejection, instance resolution for Operator-kinded classes (`tests/corpus/eval/typecheck/`)

**Depends on:** `hkt-foundation`

### hkt-mappable-appendable: Rewrite Mappable and Appendable from hardcoded to class-based

See `doc/whatif/hkt-monads.md` §Mappable Constraint, §The Typeclass Hierarchy §Mappable, §Appendable. **Spec chapters:** `doc/whatif/hkt-monads.md §The Typeclass Hierarchy`.

- [ ] Remove `Mappable` from `satisfies_constraint` hardcoded match in `src/type_unify.rs:43-47`; remove placeholder `ClassDecl` for `Mappable` from `InferState::new()` in `src/types.rs`
- [ ] Remove `Appendable` from `satisfies_constraint` hardcoded match in `src/type_unify.rs:44-47`; remove placeholder `ClassDecl` for `Appendable`
- [ ] Write `Mappable` class declaration and `MappableSeq`/`MappableRecord` instances in `stdlib/prelude.llt` per `doc/whatif/hkt-monads.md §Mappable`
- [ ] Write `Appendable` class declaration and `AppendableStr`/`AppendableSeq`/`AppendableRecord` instances in `stdlib/prelude.llt` per `doc/whatif/hkt-monads.md §Appendable`
- [ ] Extend instance resolver to handle parameterized instance heads: `AppendableSeq [Seq b]` requires matching `Seq(T)` for any `T` and extracting `b = T` to thread through method implementations (`src/type_env.rs` `resolve_instance`)
- [ ] Update `$map` and `$filter` type signatures in `src/type_env.rs` to use `Mappable f` constraint instead of hardcoded dual-dispatch; update `$concat` and `$conj` to use `Appendable a` constraint
- [ ] Tests: corpus tests for Mappable/Appendable on user-defined types, parameterized `AppendableSeq [Seq b]` instance resolution, type errors when Mappable constraint not satisfied (`tests/corpus/eval/typecheck/`)

**Depends on:** `hkt-kind-inference`

### hkt-do-macro: Implement [do] macro (explicit and inferred forms)

See `doc/whatif/hkt-monads.md` §`[do]` Inference. **Spec chapters:** `doc/whatif/hkt-monads.md §[do] Inference`.

- [ ] Implement explicit `[do monad steps...]` desugaring in `stdlib/macros.llt` (currently a stub): `[x: expr] rest` → `[monad.bind expr [fn [x] [do monad rest]]]`; non-binding step → `[monad.bind expr [fn [_] [do monad rest]]]`; terminal expression → itself; verify this passes existing `do` corpus tests
- [ ] Implement inferred `[do steps...]` form: emit a typeclass constraint token that the type checker resolves to a concrete monad dict using the inference rules (return type annotation → first binding RHS → error) before handing back to the evaluator (`stdlib/macros.llt`, `src/typecheck.rs`)
- [ ] Implement inference rule 1 in `src/typecheck.rs`: when `[do]` has no explicit monad, check the enclosing function's return type annotation for a type that unifies with `App(m, _)` for a registered `Monad m` instance
- [ ] Implement inference rule 2 in `src/typecheck.rs`: if the first binding's RHS has inferred type `App(m, a)` where `m` has a registered `Monad` instance, use that instance
- [ ] Emit a clear error when neither inference rule applies and no explicit monad is given: "cannot infer monad for [do] — add an explicit monad argument or annotate the enclosing function's return type"
- [ ] Tests: corpus tests for explicit `[do result ...]`, inferred `[do]` from return annotation, inferred `[do]` from first binding, missing-monad error, backward compatibility with pre-existing `[do monad ...]` usage (`tests/corpus/eval/`)

**Depends on:** `hkt-kind-inference`

### hkt-stdlib: Functor/Applicative/Monad/Foldable hierarchy, Maybe, generic functions

See `doc/whatif/hkt-monads.md` §The Typeclass Hierarchy, §Generic Functions. **Spec chapters:** `doc/whatif/hkt-monads.md §The Typeclass Hierarchy`, `§Generic Functions`.

- [ ] Write `Functor` class and `FunctorResult`/`FunctorSeq` instances in `stdlib/prelude.llt`
- [ ] Write `Applicative` class (extends Functor) and `ApplicativeResult`/`ApplicativeSeq` instances in `stdlib/prelude.llt`
- [ ] Write `Monad` class (extends Applicative) and `MonadResult`/`MonadSeq` instances in `stdlib/prelude.llt`
- [ ] Write `Foldable` class (`fold`, `to-seq`) and `FoldableSeq`/`FoldableRecord` instances in `stdlib/prelude.llt`; `FoldableSeq.fold = reduce`, `FoldableRecord.fold = reduce`, `to-seq` extracts element sequence
- [ ] Add `Maybe` ADT (`Maybe: [type [Some a] [None]]`), re-export `Some`/`None` constructors, write `FunctorMaybe`/`ApplicativeMaybe`/`MonadMaybe` instances in `stdlib/prelude.llt`
- [ ] Write `sequence`, `traverse`, `forM`, `when`, `liftM2` generic functions in `stdlib/prelude.llt` per `doc/whatif/hkt-monads.md §Generic Functions`; type-check all signatures end-to-end
- [ ] Tests: corpus tests for `sequence result [...]`, `traverse result fn urls`, `[do MonadMaybe ...]` with short-circuit, `when`, `liftM2`, `Foldable.fold` on Seq and Record (`tests/corpus/eval/`)

**Depends on:** `hkt-do-macro`, `hkt-mappable-appendable`

### hkt-bas: BAS extension for App type atoms and functorial subtyping

See `doc/whatif/hkt-monads.md` §Interaction with BAS. **Spec chapters:** `doc/whatif/hkt-monads.md §Interaction with BAS`.

- [ ] Extend `is_subtype` in `src/types.rs` to treat `App(f, a)` as a lattice atom: `App(f, a) <: App(f, b)` when `a <: b` and `f` is a registered `Functor` instance (covariant functorial subtyping); consult `ClassEnv` for `Functor` instances
- [ ] Implement join rule: `App(m, a) | App(m, b) <: App(m, a | b)` — derive this from the two covariance steps rather than adding a special rewrite rule
- [ ] Do NOT implement the reverse distribution `App(m, a|b) <: App(m, a) | App(m, b)` — unsound for diagonal functors
- [ ] Extend `collect_all_vars` and `apply` in `src/type_unify.rs` / `src/types.rs` to handle `App` and `Operator` variants
- [ ] Tests: corpus tests for `Result Int <: Result (Int | Str)` (covariance), `Result Int | Result Str <: Result (Int | Str)` (join), verify reverse direction is NOT accepted (`tests/corpus/eval/typecheck/`)

**Depends on:** `hkt-kind-inference`

### hkt-doc-lsp: doc/06 Type Classes section, LSP hover, error quality

See `doc/whatif/hkt-monads.md §What Would Change`. **Spec chapters:** `doc/06-type-inference.md`, `doc/whatif/hkt-monads.md`.

- [ ] Write §Type Classes formal rules section in `doc/06-type-inference.md` (deferred from `type-classes-full`): constraint generation rules, entailment checking algorithm, dictionary elaboration, instance resolution, superclass extraction — document existing `ClassEnv`/`InstanceEnv` machinery plus HKT extensions
- [ ] Add `Type::App` and `Type::Operator` display in LSP hover: `App(Result, Int)` displays as `[Result Int]`; `Operator("m")` displays as `m` in hover output (`src/lsp/analysis.rs`)
- [ ] Add go-to-definition support for class method references: `[m.bind ...]` where `m` is a Monad instance should navigate to the `bind:` field in the instance declaration (`src/lsp/analysis.rs`)
- [ ] Kind error message quality: "kind mismatch at `f@Operator`: `Int` has kind `*`, expected type constructor" — include the annotation span and a hint about the correct kind annotation
- [ ] Move accepted `doc/whatif/hkt-monads.md` to `doc/whatif/completed/hkt-monads.md`; update `doc/whatif/index.md` Accepted section with acceptance date 2026-05-11
- [ ] Tests: LSP corpus tests for hover on `Type::App` and `Type::Operator`, error message corpus tests for kind mismatches (`tests/lsp_corpus_tests.rs`, `tests/corpus/eval/errors/`)

**Depends on:** `hkt-stdlib`, `hkt-bas`
