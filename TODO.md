# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Research (requires /rnd before implementing)

- Constraint annotation syntax — should users be able to write explicit type class constraints in annotations (e.g. `@[Equatable a]`)? Currently constraints are implicit and inferred. Motivating use cases: LSP hover copy-paste principle (constraint info is advisory-only today); stdlib authors wanting to explicitly declare polymorphic interface requirements. Write design note in `doc/whatif/` first.

---

## Type System Cleanup

### prelude-type-annotations: Fix type annotations in stdlib/prelude.llt

Audit findings from 2026-05-11. Focus: public-facing functions only. Internal helpers (`-impl`, `-step`, `-check`, `sort-merge`, etc.) excluded.

- [ ] `when`/`unless` (lines 497–502): change `fn@Any` → `fn@Unknown` — the `@Any` annotation is semantically wrong; the return is either the unannotated `body` arg or `[]` (Null), which is the gradual-typing `Unknown` case, not the lattice ceiling `Any`
- [ ] `cond` (line 511): change `fn@Any` → `fn@Unknown` — delegates to `cond-impl`; returns the untyped `result` branch or `[]` (Null when no branch matches); same `Any`→`Unknown` mismatch as `when`/`unless`
- [ ] `get` (line 524): add `fn@Unknown` return annotation — currently bare `fn`; delegates to `builtin-get` whose return type is unknown at the static level; unannotated and knowable
- [ ] `get-or` (line 535): add `fn@Unknown` return annotation — bare `fn`; returns either the dict value or the `default` param, both untyped; unannotated and knowable
- [ ] `get-in` (line 543): change `fn@Any` → `fn@Unknown` — traverses nested dicts; return is a leaf value whose type is unknown; `Unknown` is the correct gradual annotation
- [ ] `get-in-or` (line 554): change `fn@Any` → `fn@Unknown` — same issue as `get-in`; on the missing-key path returns unannotated `default` param
- [ ] `zip` (line 747): change `fn@Any` → `fn@Unknown` — always returns a collection (Seq or Dict depending on inputs), but the dual-dispatch return cannot be pinned to either; `Unknown` is more honest than `Any` (lattice ceiling)
- [ ] `and`/`or` (lines 354/363): add `fn@Unknown` return annotation — both bare `fn`; `and` returns `b` or `false`, `or` returns `a` or `b`; neither is statically pinnable without union types; currently unannotated and knowable
- [ ] `find-first`/`find-first-or` (lines 833/836): add `fn@Unknown` return annotation — both bare `fn`; return single element from filtered collection whose type is unknown statically; currently unannotated and knowable
- [ ] `min`/`max` (lines 925/933): add `fn@Unknown` return annotation — both bare `fn`; return the winning element (type equals element type, unknown without parametric annotation); currently unannotated and knowable
- [ ] `between` (line 1171): add `fn@Fn` return annotation — bare `fn [lo hi]`; always returns a closure (the inner `[fn [v] ...]`); `@Fn` is the correct annotation; currently unannotated
- [ ] `non-negative`/`positive` (lines 1178/1185): add `fn@Bool` return annotation — both bare `fn [v]`; always return Bool (they delegate to `>=` and `>`); currently unannotated and clearly knowable
- [ ] `assert` (line 1095): change `fn@Bool` return annotation to `fn@Unknown` — currently annotated `@Bool` but the false path calls `[error msg]` which diverges (never returns); the true path returns literal `true`; until `Never` is in the prelude type system, `Unknown` is more accurate than claiming it always returns Bool
- [ ] `fold` (line 725): add `fn@Unknown` return annotation — bare `fn [f@Fn init xs]`; delegates to `builtin-reduce`; return type is the accumulator type (equals `init`'s type, statically unknown); currently unannotated and knowable
- [ ] `result-map`/`result-or`/`and-then`/`result-ok` (lines 1046–1073): add `fn@Unknown` return annotations to all four — all bare `fn`; all operate on Result values whose type system representation is `Unknown` until `Type::Variant` is added; currently unannotated and knowable

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

Accepted 2026-05-11. See `doc/whatif/completed/hkt-monads.md` for the full design.
Adds `Kind::Operator` (`* → *`), `Type::App`, `Type::Operator(String)`, the Functor/Applicative/Monad/Foldable/Mappable/Appendable typeclass hierarchy, Maybe ADT, generic functions (sequence, traverse, forM, when, liftM2), and inferred `[do]`.

### hkt-foundation: Kind system, App/Operator types, annotation parsing

See `doc/whatif/completed/hkt-monads.md` §Syntax Design, §Formal Type Rules, §What Would Change. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §Syntax Design`, `§Formal Type Rules`.

- [ ] Add `Kind::Operator` as notation for `* → *` — represented as `Kind::Arrow(Box::new(Kind::Type), Box::new(Kind::Type))` using the existing `Kind::Arrow` variant; no new `Kind` variant needed; document with a type alias or comment at usage sites (`src/types.rs`)
- [ ] Add `Type::App(Box<Type>, Box<Type>)` variant for type constructor application (`src/types.rs`)
- [ ] Add `Type::Operator(String)` variant for type constructor variables — distinct from `Kind::Operator`; the former is a type-level term (a named variable), the latter is a kind-level classifier (`src/types.rs`)
- [ ] Implement `PartialOrd`/`Ord` for `Type::App` and `Type::Operator` consistent with `normalize_union`'s sort order — needed for union/intersection normalization of HKT types (`src/types.rs`)
- [ ] Update ALL exhaustive `Type` match sites for new `App`/`Operator` variants:
  - `src/desugar.rs` — add arms (likely `_ => unreachable!()` since type-level only)
  - `src/eval.rs` main match — add arms (`App`/`Operator` should never reach eval; `EvalError::internal(...)`)
  - `src/eval.rs::value_matches_type` — add `Type::App(_,_) => true` and `Type::Operator(_) => true` (treat like TypeVar — defer to type checker)
  - `src/typecheck.rs` — add inference arms
  - `src/lsp/analysis.rs` — both `Type` matches in hover and `Expr` matches for `Expr::TypeApp`
  - `src/ast_dict.rs` — both `expr_to_dict` and `dict_to_ast` directions
  - `src/type_unify.rs` — `unify`, `is_subtype` placeholder arms
  - `src/type_env.rs` — `Display for Type` impl (add `App`/`Operator` display arms)
- [ ] Update `Type` tree-walker functions in `src/type_unify.rs` — **required in this sprint before UNIFY-APP/UNIFY-OPERATOR can be sound**: `collect_all_vars` (App/Operator arms — recurse into both sub-types), `collect_all_vars_check_occurs` (same), `collect_all_vars_vec` (same), `has_inference_vars` (same), `lower_levels_check_occurs` (same — needed for occurs check soundness in UNIFY-APP)
- [ ] Update `Substitution::apply_type` in `src/type_unify.rs` — add `App(f, a)` arm (substitute through both constructor and argument recursively) and `Operator(m)` arm (look up `m` in substitution map, return binding or clone)
- [ ] Add `Display` impls for `Type::App` as `"[{f} {a}]"` and `Type::Operator(name)` as `"{name}"` in `src/type_env.rs` `Display for Type` match
- [ ] Add `UNIFY-OPERATOR` rule to `src/type_unify.rs`: `unify(Operator(m), T) = [m ↦ T]` with occurs check via `collect_all_vars_check_occurs`; symmetric `unify(T, Operator(m)) = [m ↦ T]`
- [ ] Add `UNIFY-APP` rule to `src/type_unify.rs`: `unify(App(f₁, a₁), App(f₂, a₂))` — unify constructors `f₁`/`f₂` first, apply resulting substitution, then unify arguments; return composed substitution
- [ ] Add `Expr::TypeApp(Box<Expr>, Box<Expr>)` to AST (`src/ast.rs`); add eval handler in `src/eval.rs` returning `EvalError::internal("TypeApp is a type annotation node and cannot be evaluated")` — this node exists only in annotation positions
- [ ] Recognize `@Operator` annotation: in `src/typecheck_annot.rs` `resolve_type_name`, when name is `"Operator"`, emit `TypeError` with message "`Operator` is a kind annotation — use `f@Operator` on a class parameter, not `@Operator` as a standalone type"
- [ ] In annotation positions, parse `[f a]` (no colons) as `Expr::TypeApp(f, a)` in `resolve_type_expr` (`src/typecheck_annot.rs`) when `f` is an Operator-kinded type variable (from `kind_env`) or a user parameterized type alias; builtins (`Seq`, `Map`) keep existing `@Seq@T` path via `resolve_annotated` — `[Seq Int]` in annotation position resolves via existing parameterized-alias path, not `Expr::TypeApp`
- [ ] Extend `class` declaration parsing to accept `extends [SuperClass param]` clause: change `Expr::ClassDecl.superclasses` from `Vec<String>` → `Vec<(String, String)>` in `src/ast.rs`; update ALL match sites that destructure `ClassDecl`: `src/expand.rs`, `src/ast_dict.rs` (both directions), `src/formatter.rs` (**must** emit `extends [SuperClass param]` for non-empty superclasses to prevent data-loss round-trip — currently `superclasses: _` silently drops them), `src/typecheck.rs`, `src/eval.rs`, `src/parser.rs`
- [ ] Add `kind_env: HashMap<String, Kind>` to `InferState` (`src/types.rs`) to store kind assignments for type constructor variables; populate during class method signature processing; make available to `resolve_type_name` for Operator-variable lookup (`src/typecheck.rs`)
- [ ] Update `ClassDecl` construction in `src/typecheck.rs` (around line 1719): assign `Kind::Arrow(Type, Type)` (Operator kind) when a class parameter is annotated `@Operator` or constrained by an Operator-kinded class, instead of always `Kind::Type`
- [ ] Tests: corpus tests for `@[m a]` type application, `extends` syntax in class declarations, `Type::App` display `[Result Int]`, kind annotation `@Operator` on bare type produces correct error, `UNIFY-APP`/`UNIFY-OPERATOR` unit tests in `src/type_unify.rs` (`tests/corpus/eval/typecheck/`, `src/type_unify.rs`)

### hkt-kind-inference: Kind checking pass and Operator-kinded class resolution

See `doc/whatif/completed/hkt-monads.md` §Formal Type Rules §Kind Checking, §Typeclass Resolution for HKT. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §Formal Type Rules`.

- [ ] Add kind inference pre-pass in `src/typecheck.rs` before HM inference: walk class method signatures, look up parameter kinds from `InferState.kind_env`; assign `Kind::Arrow(Type, Type)` (Operator) to parameters annotated `@Operator` or constrained by an Operator-kinded class (`Monad`, `Functor`, `Mappable`, etc.)
- [ ] Implement `KIND-OPERATOR` rule: validate `App(f, a)` during annotation resolution — look up `f` in `kind_env`; if `f : Operator` and `a : *`, the application is valid; if `f : *`, emit `TypeError` "kind mismatch: expected type constructor (`* → *`), got concrete type (`*`)" (`src/typecheck.rs`, `src/typecheck_annot.rs`)
- [ ] Enforce rank-1 restriction: reject `App(Operator("f"), Operator("g"))` where both `f` and `g` are Operator-kinded in `kind_env` — emit `TypeError` "rank-2 type constructor application is not supported" at the annotation span (`src/typecheck.rs`)
- [ ] Extend `ClassEnv` lookup in `src/type_env.rs` to handle Operator-kinded class parameters: when resolving constraint `C m` where `m : Operator`, match instance entries by unifying the instance head against `App(m, _)` using `UNIFY-APP`; extend `resolve_instance` to freshen free type variables in the instance type before unification (via `instantiate_at_level`) and capture the resulting substitution bindings — currently `temp_subst` is discarded after the `is_ok()` check; instead return bindings and apply them to instance method implementations so `b = T` is correctly threaded through parameterized heads like `AppendableSeq [Seq b]`
- [ ] Add `App` type inference in `src/typecheck.rs`: when a binding has inferred type `App(Operator("m"), a)`, apply `UNIFY-OPERATOR` to bind `m` against known instance heads; update `InferState.subst`
- [ ] Normalize at instance resolution time (`src/typecheck_annot.rs`): when Operator variable resolves to a builtin, map `App(Seq_ctor, T) → Type::Seq(T)`; for `Map` (arity 2), represent as `App(App(Map_ctor, K), V)` (curried) and normalize to `Type::Map(K, V)` when both arguments are known; `App(Result_ctor, T)` has no dedicated `Type` variant — leave as `App`
- [ ] Assign new error code (e.g. `E091`) for kind mismatch errors in `src/error.rs`; add to `doc/10-errors.md` error code tables
- [ ] Tests: corpus tests for kind mismatch errors (with `[E091]` code), `App(Result, Int)` inferred from `[Ok 42]`, rank-1 violation rejection, Operator-kinded class constraint resolution (`tests/corpus/eval/typecheck/`)

**Depends on:** `hkt-foundation`

### hkt-mappable-appendable: Rewrite Mappable and Appendable from hardcoded to class-based

See `doc/whatif/completed/hkt-monads.md` §Mappable Constraint, §The Typeclass Hierarchy §Mappable, §Appendable. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §The Typeclass Hierarchy`.

- [ ] Extend `resolve_instance` in `src/type_env.rs` to support parameterized instance heads — freshen all free type variables in `inst.instance_type` via `instantiate_at_level` before unification; capture the resulting substitution (currently `temp_subst` is discarded after `is_ok()`) and apply it to the instance's method implementations so `b = T` is threaded through `append` and `empty` in `AppendableSeq [Seq b]`; `AppendableRecord` matches `Type::Record(_)` for any row (open or closed)
- [ ] Write `Mappable` class declaration (`[class [f@Operator] ...]`) and `MappableSeq`/`MappableRecord` instances in `stdlib/prelude.llt`; update `ClassDecl` kind annotation for Mappable param from `Kind::Type` to `Kind::Arrow(Type,Type)` in `InferState::new()` (`src/types.rs`)
- [ ] Write `Appendable` class declaration (`[class [a] ...]`, kind-`*`) and `AppendableStr`/`AppendableSeq`/`AppendableRecord` instances in `stdlib/prelude.llt` — `AppendableSeq` has parameterized head `[Seq b]`, relying on `resolve_instance` freshening fix above
- [ ] Remove `Mappable` from `satisfies_constraint` hardcoded match (`src/type_unify.rs:43-47`) and remove placeholder `ClassDecl` from `InferState::new()` — **only after** verifying `resolve_instance` handles Operator-kinded Mappable end-to-end (run existing Mappable corpus tests first to confirm no regression)
- [ ] Remove `Appendable` from `satisfies_constraint` hardcoded match — same gate condition as Mappable removal
- [ ] Update `$map` and `$filter` type signatures in `src/type_env.rs` to use `Mappable f` constraint (supersedes `builtin-type-audit`'s `Unknown→Any` change for these entries); update `$concat`/`$conj` to use `Appendable a` constraint
- [ ] Write `Equatable` class (`[class [a] [= [fn@Bool [a a]]] [not= [fn@Bool [a a]]]]`) and instances for `Int`, `Str`, `Bool`, `Float` in `stdlib/prelude.llt`; remove `Equatable` from `satisfies_constraint` hardcoded match in `src/type_unify.rs`
- [ ] Write `Comparable` class (extends Equatable, `[< [fn@Bool [a a]]]` plus `<=`/`>`/`>=`) and instances for `Int`, `Str`, `Float` in `stdlib/prelude.llt`; remove `Comparable` from `satisfies_constraint`
- [ ] Write `Showable` class (`[class [a] [show [fn@Str [a]]]]`) and instances for `Int`, `Str`, `Bool`, `Float`, `Null` in `stdlib/prelude.llt`; remove `Showable` from `satisfies_constraint`; `Numeric` remains hardcoded — its mixed-type arithmetic (`Int + Float → Float`) requires multi-parameter type classes (out of scope)
- [ ] Tests: Mappable on user-defined type (success), `map` on non-Mappable `Int` (constraint violation), `concat` on mixed Appendable/non-Appendable (error), `AppendableSeq [Seq b]` resolving for different element types, `AppendableStr` instance works for string concat, `Equatable`/`Comparable`/`Showable` constraints on user types (`tests/corpus/eval/typecheck/`)

**Depends on:** `hkt-kind-inference`

### hkt-do-macro: Implement [do] macro — explicit form first, inferred form second

See `doc/whatif/completed/hkt-monads.md` §`[do]` Inference. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §[do] Inference`.

Note: The explicit `[do monad steps...]` desugaring has no HKT dependency and should be implemented and tested independently first.

- [ ] Implement explicit `[do monad steps...]` desugaring in `stdlib/macros.llt` using iterative `reduce` (not recursive macro-in-macro-output): classify each step as binding (`[x: expr]` — a single-named-entry dict in the AST) or non-binding (plain expression) by inspecting the step AST dict shape; terminal step (last positional arg after monad) returns as-is; desugar bindings to `[monad.bind expr [fn [x] ...]]`, non-bindings to `[monad.bind expr [fn [_] ...]]`; `[do monad]` with no steps → `[monad.pure []]`; `[do]` with zero args → error
- [ ] Implement inferred `[do steps...]` form: at macro-expand time emit `[do %do-infer steps...]` with a sentinel variable; in `src/typecheck.rs` `infer_expr`, when the `[do]` form has sentinel monad `%do-infer`, apply inference rules: (1) check enclosing function's declared return type annotation (thread via a new `expected_return: Option<Type>` context parameter in `infer_expr`) for a type unifying with `App(m, _)` for a registered Monad instance; (2) if not found, infer the first binding RHS type and check if it is `App(m, a)` for a known Monad; (3) substitute resolved monad dict for `%do-infer`; (4) if unresolved, emit error
- [ ] Emit clear error when monad cannot be inferred: "cannot infer monad for `[do]` — add an explicit monad argument or annotate the enclosing function's return type"
- [ ] Tests: `[do result ...]` three-step success, `[Err "fail"]` propagation (short-circuit without evaluating remaining steps), explicit `[do]` with any dict carrying `bind:` field (backward compat), inferred `[do]` from `@Result` return annotation, inferred `[do]` from first binding type, missing-monad error, `[do monad]` with no steps → `[monad.pure []]` (`tests/corpus/eval/`)

**Depends on:** `hkt-kind-inference` (inferred form only; explicit form can proceed after `hkt-foundation`)

### hkt-stdlib: Functor/Applicative/Monad/Foldable hierarchy, Maybe, generic functions

See `doc/whatif/completed/hkt-monads.md` §The Typeclass Hierarchy, §Generic Functions. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §The Typeclass Hierarchy`, `§Generic Functions`.

- [ ] Write `Functor` class (`[class [f@Operator] [fmap: [fn@[f b] [fn@b [a]  [f a]]]]]`) and `FunctorResult`/`FunctorSeq` instances in `stdlib/prelude.llt`
- [ ] Write `Applicative` class (extends Functor, `pure` + `lift2`) and `ApplicativeResult`/`ApplicativeSeq` instances in `stdlib/prelude.llt`
- [ ] Write `Monad` class (extends Applicative, `bind`) and `MonadResult`/`MonadSeq` instances in `stdlib/prelude.llt`
- [ ] Write `Foldable` class (`fold: [fn@b [fn@b [b a]  b  [t a]]]`, `to-seq`) and `FoldableSeq`/`FoldableRecord`/`FoldableResult` instances; `FoldableSeq.fold = reduce`, `FoldableRecord.fold = reduce`; `FoldableResult.fold: [fn [f init r] [and-then r [fn [a] [f init a]]]]`; `FoldableResult.to-seq: [fn [r] [match r [Ok xs] xs [Err _] []]]`
- [ ] Add `Maybe` ADT — verify exact parser syntax against `Result: [type [Ok a] [Err String]]` at `stdlib/prelude.llt:1031`; re-export `Some: Some` and `None: None` following the `Ok: Ok` / `Err: Err` pattern at lines 1036-1039; write `FunctorMaybe`/`ApplicativeMaybe`/`MonadMaybe` instances
- [ ] Write `Traversable` class (extends Functor, extends Foldable — `extends` takes `Vec<(String,String)>` so multiple superclasses work) and `TraversableSeq`/`TraversableResult`/`TraversableMaybe` instances in `stdlib/prelude.llt` per `doc/whatif/completed/hkt-monads.md §Traversable`
- [ ] Rewrite `sequence` and `traverse` as generic over any `Traversable` container: `sequence: [fn@[f [t a]] [f@Monad t@Traversable xs@[t [f a]]] [traverse f [fn [x] x] xs]]`; `traverse: [fn@[f [t b]] [f@Monad t@Traversable fn@[f b] [a] xs@[t a]] [t.traverse f xs]]` — replaces the Seq-specific implementations
- [ ] Write `forM`, `when`, `liftM2` generic functions per `doc/whatif/completed/hkt-monads.md §Generic Functions`
- [ ] Verify `sequence` short-circuits correctly on first `Err`/`None` via `Traversable` instances (left-fold via `m.bind` propagates failure; verify no evaluation of subsequent elements after failure)
- [ ] Tests: `sequence result [[Ok 1] [Err "fail"] [Ok 3]]` → `[Err "fail"]` (short-circuit), `traverse result f [1 2 3]` success, `sequence` over `TraversableResult` (Ok/Err), `traverse` over `TraversableMaybe` (Some/None short-circuit), `forM` composition, `when false failing-action` (action not evaluated), `liftM2`, `[do MonadMaybe ...]` with `[None]` short-circuit, `FoldableSeq.fold` equals `reduce` for identical inputs, `FoldableResult` fold on `Ok`/`Err` (`tests/corpus/eval/`)

**Depends on:** `hkt-do-macro`, `hkt-mappable-appendable`

### hkt-bas: BAS extension for App type atoms and functorial subtyping

See `doc/whatif/completed/hkt-monads.md` §Interaction with BAS. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §Interaction with BAS`.

- [ ] Extend `is_subtype` in `src/types.rs` with covariant functorial subtyping: add arm `(App(f₁, a), App(f₂, b))` — when `f₁ == f₂` (same constructor) and `is_subtype(a, b)` holds, the application is a subtype; no `ClassEnv` access needed since all type constructors we use are covariant in their argument by declaration; `is_subtype` signature does not change
- [ ] The join rule `App(m, a) | App(m, b) <: App(m, a|b)` is derived automatically from two covariance steps via the existing `UNION-ELIM` rule — verify via tests, no special rewrite rule needed
- [ ] Do NOT implement `App(m, a|b) <: App(m, a) | App(m, b)` — unsound for diagonal functors
- [ ] Verify `collect_all_vars`/`collect_all_vars_check_occurs`/`collect_all_vars_vec`/`has_inference_vars`/`lower_levels_check_occurs`/`apply_type` handle `App`/`Operator` correctly in the BAS subtype context — these were updated in `hkt-foundation`; run all existing BAS corpus tests after adding the `is_subtype` arm to confirm no regressions
- [ ] Tests: `App(Result, Int) <: App(Result, Int|Str)` (covariance), `App(Result,Int)|App(Result,Str) <: App(Result,Int|Str)` (join via UNION-ELIM), verify reverse direction `App(Result,Int|Str) <: App(Result,Int)` is NOT accepted, mismatched constructors rejected (`tests/corpus/eval/typecheck/`)

**Depends on:** `hkt-foundation`, `hkt-kind-inference`

### hkt-doc-lsp: doc/06 Type Classes section, LSP hover, error quality

See `doc/whatif/completed/hkt-monads.md §What Would Change`. **Spec chapters:** `doc/06-type-inference.md`, `doc/whatif/completed/hkt-monads.md`.

- [x] Move `doc/whatif/hkt-monads.md` to `doc/whatif/completed/hkt-monads.md` — already done
- [x] Update `doc/whatif/index.md` Accepted section with acceptance date 2026-05-11 — already done
- [ ] Write §Type Classes formal rules section in `doc/06-type-inference.md`: `satisfies_constraint` fixed-instance table (pre-HKT), `ClassDecl`/`InstanceDecl` AST shapes and method signatures, constraint entailment algorithm (`is_entailed` in `src/type_unify.rs`), superclass chain (Monad extends Applicative extends Functor), `UNIFY-OPERATOR`/`UNIFY-APP` rules, `KIND-OPERATOR`/`KIND-CLASS-PARAM` rules, parameterized instance head resolution
- [ ] `Type::App` and `Type::Operator` Display was added in `hkt-foundation` (`src/type_env.rs`) — verify LSP hover shows `[Result Int]` for `App(Result, Int)` by running hover corpus tests; the display flows through `TypeScheme.body.to_string()` automatically once Display is in place
- [ ] Add `Expr::TypeApp` arm to `hover_at_expr` in `src/lsp/analysis.rs`: when cursor is on a TypeApp node, display the resolved `App` type from the type map (same pattern as `Expr::Annotated` hover handling)
- [ ] Kind error message quality: include annotation span, the mismatched kinds, and a hint — "kind mismatch at `f`: `Int` has kind `*`, expected type constructor (`* → *`) — annotate as `f@Operator`"
- [ ] Add kind mismatch error code E091 to `doc/10-errors.md` all three tables (variant catalog, codes table, categories table)
- [ ] Tests: LSP hover corpus tests for `Type::App` display, kind mismatch error corpus tests with `[E091]` prefix (`tests/lsp_corpus_tests.rs`, `tests/corpus/eval/errors/`)

**Depends on:** `hkt-stdlib`, `hkt-bas`
