# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Research (requires /rnd before implementing)

- [x] Research constraint annotations — see `doc/whatif/constraint-annotations.md`. Decision: `fn@[...]` becomes a named-key metadata dict with `return:`, `constraint:`, and `doc:` keys; `constraint: [a: Comparable]` uses binding syntax (lowercase TypeVar key, uppercase class value); `fn@Type` shorthand permanent.

- [x] Research union annotations with named TypeVars — verified: `ann_mapping` propagates through all positional union entries in `resolve_annotation` → `resolve_type_expr` → `resolve_type_name`; `a` in `fn@[a Null]` shares the same TypeVar as `body@a`. **This is a sprint, not research.** Follow-up tasks added to `prelude-type-annotations` below. Prerequisite: `constraint-annotations` sprint (fixes `fn@[...]` positional-union path).

- [x] Research row-access types for `get`/`get-in` — merged into `doc/whatif/completed/hkt-monads.md §Field Access Typing`. Design: `HasField` qualified-type constraint (G-J-for-BAS); `Kind::Label`; `[HAS-FIELD-REC/UNION/INTER/TOP]` BAS rules; `[GET]`/`[GET-IN]` type rules; label-polymorphic `get`/`get-in`; Castagna (2023) formally proves union distribution. Implementation lands in `hkt-foundation` + `hkt-mappable-appendable`.

- [x] Research LSP prelude go-to-definition — `Span` carries no file path but `find_definition` already returns `(Uri, Span)` as separate values; `llt_span_to_lsp_range` takes source text separately, so path-less spans work fine. Approach: parse prelude once at LSP startup using the embedded `include_str!()` source; cache the `Spanned<File>` AST; extend `definition_at()` to search it after local/include miss; resolve URI via `find_libdir_path().join("prelude.llt")` + `file_path_to_uri()`. **This is a sprint.** Tasks added to `lsp-gaps`.

- [x] Research inference completeness — see `doc/whatif/inference-completeness.md`. Design: SCC-based binding group analysis (Tarjan + topological sort within DICT-GEN) eliminates letrec monomorphism and nested dict polymorphism simultaneously; no value restriction (pure language); polymorphic recursion rejected with clear error; variadic params typed as `Seq(T)` with call-site unification; typeclass-based heterogeneous variadics (FormatResult pattern) for printf-style use cases. Three related gaps in tinct's HM inference engine, all addressable together: (1) **letrec monomorphism** — all entries in a letrec group are monomorphic with respect to each other; forward references see a fresh TypeVar rather than a generalized scheme; can DICT-GEN be extended to generalize entries independently? (Mycroft 1984, Kiselyov 2013 levels); (2) **nested dict let-polymorphism** — only top-level dict entries receive DICT-GEN Pass 4 generalization; inner entries remain at the outer level; can inner entries be generalized independently while respecting letrec scoping? (3) **typed variadic parameters** — `...args` is typed `Unknown` because the runtime collects remaining args into an Int-keyed Dict; can variadics collect into a typed `Seq[T]` instead, requiring a runtime representation change?

- [x] Research advanced typeclass extensions — see `doc/whatif/advanced-typeclasses.md`. Design: 3-parameter `Add a b c | (a,b)→c` MPTC with functional dependencies for precise mixed-mode arithmetic; row-level constraint propagation via BAS intersection distribution ([CONSTRAIN-FIELD/INTER/UNION]); runtime ClassEnv dispatch extending primitive operator builtins to user-defined instances; all three extend the same Constraint infrastructure and share the ClassEnv registry. Three tightly-interlinked extensions to the typeclass system beyond the HKT baseline, all extending the same `Constraint` infrastructure: (1) **multi-parameter type classes for Numeric** — `[+ Int Float] → Float` requires MPTCs; `Numeric` stays hardcoded because single-parameter classes cannot express coercion typing (Jones 1995 functional dependencies, Peyton Jones et al. 1997 type improvement); (2) **row-level constraints** — `Equatable [name: a ...]` (all fields satisfy a constraint) requires row-level constraint propagation under BAS; what does `Homogeneous` look like over BAS intersections? (Gaster & Jones 1996, PureScript); (3) **runtime typeclass dispatch** — user-defined instances cannot intercept primitive operators (`=`, `<`, `str`) because builtins dispatch via Rust type inspection, not via instance dictionaries; what would dictionary translation (Wadler & Blott 1989, Jones 1995) look like for tinct?

---

## Type System Cleanup

### infer-fn-typevar: Fix unannotated param TypeVar inference and gated prelude follow-ups

These two items were gated out of `builtin-type-audit` because the `infer_fn` TypeVar fix is a significant behavior change that requires its own audit sprint; batch A prelude annotations depend on it landing first.

- [ ] `infer_fn` unannotated params: change `None => Ok(Type::Unknown)` (line 3074 `src/typecheck.rs`) to `None => Ok(state.new_type_var(span))` — unannotated params should get fresh TypeVars for proper HM inference, not Unknown (gradual opt-out). This enables constraint propagation (e.g. `[fn [a b] [= a b]]` infers `Equatable a => Fn@Bool [a a]`) and LSP hover shows `a` not `Unknown`. This is a significant behavior change — audit for test breakage.
- [ ] **Prelude follow-ups (batch A)** — gate on BOTH `error → Never` AND `infer_fn` TypeVar fix above landing first:
  - `fold` (prelude.llt:725): change `fn@Unknown` → `fn@a [f@Fn init@a xs]` — `a` in `fn@a` and `init@a` binds return type to the accumulator type (`stdlib/prelude.llt`)
  - `assert` (prelude.llt:1095): change `fn@Unknown` → `fn@Bool` — once `error` is typed `Never`, inference produces `Bool | Never = Bool`, making `@Bool` correct (`stdlib/prelude.llt`)

---

## Type Quality

Two-tier Unknown diagnostic policy: explicitly annotated `@Unknown` is silenced in default mode and warned in `--strict`; inferred `Unknown` is warned in default mode and errors in `--strict`. The same warning channel also surfaces over-broad annotations where inference determines the type is narrower than declared. Both sprints are independent of HKT and can land at any time.

### unknown-diagnostics: Unknown and over-broad annotation diagnostics

Post-processing pass after `typecheck_file` completes: walk each binding's final `TypeScheme` in the type map, classify each diagnostic, and emit `TypeDiagnostic` at the appropriate level (Info/Warn/Err). Also detects over-broad annotations where inference produces a more specific type than declared.

**Diagnostic classification (before `--strict` bump):**
- Explicit `@Unknown` annotation / `[@Unknown expr]` TypeAssert → **Info** (you chose it; `--strict` bumps to Warn)
- Over-broad annotation (`fn@Number` when inference gives `Int`, etc.) → **Info** (a suggestion; `--strict` bumps to Warn)
- Inferred Unknown (type resolved to Unknown without the user asking for it) → **Warn** (you didn't choose this; `--strict` bumps to Err)

`--strict` applies the level bump from `type-warning-channel` uniformly — no special-casing per diagnostic.

- [x] Add post-processing function `scan_type_quality(type_map, ast, diagnostics: &mut Vec<TypeDiagnostic>)` in `src/typecheck.rs`: called after `typecheck_file` completes, receives the type map and original AST; emits diagnostics at base level (Info/Warn), `--strict` bump applied at CLI/LSP layer (`src/typecheck.rs`)
- [x] Unknown detection: for each binding's `TypeScheme`, walk all type positions (return type, param types, dict entry types, intermediate types); check if `Unknown` appears; for each occurrence, inspect the original AST annotation — if `@Unknown` was explicitly written, mark as "explicit"; otherwise mark as "inferred" (`src/typecheck.rs`)
- [ ] TypeAssert detection: for `[@Unknown expr]` TypeAssert nodes in the AST, treat the same as an explicit `@Unknown` annotation — silent (non-strict) / warn (strict) (`src/typecheck.rs`)
- [ ] Emit Unknown diagnostics: inferred Unknown → `TypeDiagnostic { level: Warn }` (bumped to Err by `--strict`); explicit Unknown → `TypeDiagnostic { level: Info }` (bumped to Warn by `--strict`) (`src/typecheck.rs`)
- [ ] Over-broad annotation detection: for each binding with a declared return type annotation and an inferred type, check `is_subtype(inferred, declared) && !is_subtype(declared, inferred)`; when true, emit `TypeDiagnostic { level: Info }` suggesting the inferred type as the tighter annotation (bumped to Warn by `--strict`) (`src/typecheck.rs`, `src/type_unify.rs`)
- [ ] Over-broad detection covers: `fn@Number` when body infers `Int`; `param@Dict` when inference constrains to a specific record; `@Top` / `@Any` when a precise type is inferred; union annotations `@[Int String]` when inference produces only one branch (`src/typecheck.rs`)
- [x] Wire `scan_type_quality` into `typecheck_file`; pass the `Vec<TypeDiagnostic>` through the return; `--strict` bump is applied at emission time in the CLI/LSP layer, not in the scanner itself — the scanner always emits at the base level (`src/typecheck.rs`, `src/main.rs`)
- [x] Tests: corpus tests with `=== warn` sections for: inferred Unknown warns; explicit `@Unknown` silent in default; explicit `@Unknown` warns in `--strict`; inferred Unknown errors in `--strict`; `[@Unknown expr]` same as explicit; `fn@Number` with `Int` body warns "consider @Int"; `param@Dict` with specific record warns; `--strict` escalation (`tests/corpus/eval/typecheck/`)

**Depends on:** `type-warning-channel`

---

## Higher-Kinded Types

Accepted 2026-05-11. See `doc/whatif/completed/hkt-monads.md` for the full design.
Adds `Kind::Operator` (`* → *`), `Kind::Label`, `Type::App`/`Type::Operator`, the Functor/Applicative/Monad/Foldable/Traversable/Mappable/Appendable typeclass hierarchy, Maybe ADT, `HasField` qualified-type constraint for precise `get`/`get-in` typing, generic functions (sequence, traverse, forM, when, liftM2), and inferred `[do]`.

### label-annotation-syntax: Fix label-kinded TypeVar annotation and remove explicit HasField from user code

Three design corrections discovered after `hkt-field-access` was implemented:
(1) `key@"l"` (string literal) is the wrong syntax — Label-kinded TypeVars have two correct forms depending on whether the name is needed elsewhere in the signature;
(2) `constraint: [HasField l d a]` is both malformed and wrong — HasField is never user-written, it is generated by the type checker from the label annotation;
(3) For `get`/`get-or`, the label TypeVar name is never referenced by the user — `key@Label` (anonymous, parallel to `f@Operator`) is sufficient; `key@[label: l]` (named) is only needed when the same label must appear in multiple type positions.

**Two annotation forms for Label-kinded TypeVars:**
- `key@Label` — anonymous; type checker generates a fresh label TypeVar internally; HasField constraint generated automatically; the label name is never visible to the user. Use when the label TypeVar is not referenced elsewhere in the type.
- `key@[label: l]` — named; binds label TypeVar `l` in the type scheme; use when the same label must appear in multiple positions (e.g. two parameters that must access the same field, or a return annotation that references the label).

- [x] Add `@Label` simple annotation form to `resolve_type_name` in `src/typecheck_annot.rs`: when annotation is `Simple("Label")`, create a fresh anonymous Label-kinded TypeVar (system-generated name), register `kind_env[fresh] = Kind::Label`; parallel to `@Operator` which creates an anonymous Operator-kinded TypeVar (`src/typecheck_annot.rs`)
- [x] Add `[label: name]` property dict form to the annotation resolver: when a `PropertyDict` annotation has exactly one entry with key `label` and a bare-name value, create a named Label-kinded TypeVar, register it in `kind_env` and `ann_mapping`; use when the label TypeVar must be referenced elsewhere in the type scheme (`src/typecheck_annot.rs`)
- [ ] Remove the `key@"l"` string-literal mechanism for Label TypeVars from `src/typecheck_annot.rs` — it was introduced in `hkt-field-access` and has no users outside that sprint; restore whatever pre-hkt-field-access behavior existed for string literals in annotation position (i.e. remove the code that was added, do nothing special) (`src/typecheck_annot.rs`)
- [ ] Update `stdlib/prelude.llt` `get`/`get-or` annotations: use the anonymous form since the label TypeVar is never referenced by name; remove `constraint: [HasField l d a]` entirely; correct annotations: `get: [fn@[return: a] [key@Label  dict@d] ...]` and `get-or: [fn@[return: a] [key@Label  dict@d  default@a] ...]` (`stdlib/prelude.llt`)
- [ ] Update `src/type_env.rs` scheme registration for `get`/`get-or` to match the anonymous label form; the Rust-side scheme stores the HasField constraint as a generated constraint, not user-written
- [x] Update `doc/whatif/completed/hkt-monads.md §Field Access Typing` and `doc/06-type-inference.md §HasField`: document both `@Label` (anonymous) and `@[label: l]` (named) forms with examples; replace `key@"l"` throughout; clarify HasField is never user-written
- [x] Remove the stale note at the bottom of `hkt-field-access` sprint about `constraint-annotations` dependency for HasField syntax — both the dependency and the HasField annotation syntax were incorrect
- [x] Tests: `key@Label` generates HasField constraint and returns precise field type; `key@[label: l]` where same `l` is used in two parameters works; `get`/`get-or` return precise types at call sites with string literal keys (`tests/corpus/eval/typecheck/`, `tests/lsp_corpus_tests.rs`)

### hkt-do-macro-explicit: Implement [do] macro — explicit form

See `doc/whatif/completed/hkt-monads.md` §`[do]` Inference. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §[do] Inference`.

The explicit `[do monad steps...]` form has **no HKT dependency** — it desugars to `monad.bind` field access on a plain dict. It can land before `hkt-kind-inference` completes. The inferred `[do steps...]` form (no explicit monad arg) requires `App` type inference from `hkt-kind-inference` and is a separate follow-on task in `hkt-do-macro-inferred`.

- [ ] Implement explicit `[do monad steps...]` desugaring in `stdlib/macros.llt` via the existing `STDLIB_MACROS` registration path (no `stdlib-defmacro` needed): classify each step as binding (`[name: expr]`) or non-binding (bare expression) by inspecting the AST dict shape; bindings → `[monad.bind expr [fn [name] <rest>]]`; non-bindings → `[monad.bind expr [fn [_] <rest>]]`; last step is the return value with no wrapping; `[do monad]` with no steps → `[monad.pure []]`; `[do]` with zero args → error (`stdlib/macros.llt`, `src/expand.rs`)
- [ ] Tests: `[do result [r: [fetch ...]]]` three-step success, `[Err "fail"]` propagation (short-circuit), `[do]` with any `bind:`-carrying dict (not just Result), `[do monad]` no-steps → `[monad.pure []]`, zero-args error (`tests/corpus/eval/`)

### hkt-do-macro-inferred: [do] macro — inferred monad form

The inferred `[do steps...]` form (monad argument omitted, inferred from return type or first binding). Requires `hkt-kind-inference` to provide `App` type inference and `kind_env`-based Monad class lookup.

- [ ] Add `expected_return: Option<Type>` field to `InferState` in `src/types.rs:1590` (alongside `kind_env: HashMap<String, Kind>`); set by `infer_fn` before descending into fn body when explicit return annotation is present; avoids cascading `infer_expr` signature changes (`src/types.rs`)
- [ ] In `src/expand.rs`: when `[do]` has no explicit monad arg, emit `[do %do-infer steps...]` sentinel — `Expr::VarRef("%do-infer")` as the monad argument; runtime never sees this sentinel — it is resolved and substituted by the type checker before eval (`src/expand.rs`)
- [ ] In `src/typecheck.rs` `infer_expr` for `[do]`: when monad is `VarRef("%do-infer")`, resolve monad via: (1) `state.expected_return` unified against `App(m, _)` for a registered Monad class; (2) first binding RHS type `App(m, a)` for a known Monad; (3) emit "cannot infer monad — add explicit monad arg or annotate return type" on failure; substitute resolved monad name into the desugared `[monad.bind ...]` chain before evaluation (`src/typecheck.rs`)
- [ ] Tests: inferred `[do]` from `fn@Result` return annotation, inferred from first binding type, unresolvable monad error, `[do]` inside HKT-generic function (`tests/corpus/eval/`)

**Depends on:** `hkt-kind-inference`

### hkt-mappable-appendable: Rewrite Mappable and Appendable from hardcoded to class-based

See `doc/whatif/completed/hkt-monads.md` §The Typeclass Hierarchy §Mappable, §Appendable. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §The Typeclass Hierarchy`.

`hkt-kind-inference` delivers: (1) class param annotations parsed and wired to `kind_env` — `[Mappable: [class [f@Operator] ...]]` now works; (2) `@[f a]` in annotation position produces `Type::App(f, a)` — instance method type signatures like `[fn@[f b] [[f a]]]` are typeable. This sprint builds on those two foundations.

**Phase 1 — resolve_instance freshening (enables parameterized instance heads):**
- [x] Fix `resolve_instance` in `src/type_env.rs`: freshen all free type vars in `inst.instance_type` via `instantiate_at_level` before unification — current code does NOT do this, causing `b` in `AppendableSeq [Seq b]` to leak across call sites; capture `temp_subst` bindings after successful unification (currently discarded after `is_ok()` check); apply `temp_subst` to the instance's method implementations so the concrete element type `T` threads through `append`/`empty`; this fix is general — it enables any parameterized instance head, not only Operator-kinded (`src/type_env.rs`)

**Phase 2 — Mappable migration (Operator-kinded; needs kind_env wiring from hkt-kind-inference):**
- [ ] Write `Mappable` class + `MappableSeq`/`MappableRecord` instances in `stdlib/prelude.llt`; `f@Operator` on the class param now works after hkt-kind-inference (`stdlib/prelude.llt`)
- [ ] Remove hardcoded `Mappable` placeholder `ClassDecl` from `InferState::new()` in `src/types.rs` — the class is now declared in prelude and registered via normal class-loading; also remove `Mappable` from the `satisfies_constraint` hardcoded match in `src/typecheck.rs` only after end-to-end verification that `map` on a user-defined `Mappable` type works (`src/types.rs`, `src/typecheck.rs`)
- [ ] Update `$map`/`$filter` type signatures in `src/type_env.rs` to use `Mappable f` constraint instead of hardcoded dual-dispatch (`src/type_env.rs`)

**Phase 3 — Appendable migration (Kind::Type; simpler, no Operator dependency):**
- [ ] Write `Appendable` class (kind-`*`) + `AppendableStr`/`AppendableRecord` instances + parameterized `AppendableSeq [Seq b]` instance (relies on resolve_instance freshening) in `stdlib/prelude.llt` (`stdlib/prelude.llt`)
- [ ] Remove `Appendable` from `satisfies_constraint` hardcoded match; update `$concat`/`$conj` type sigs in `src/type_env.rs` to use `Appendable a` (`src/typecheck.rs`, `src/type_env.rs`)

**Phase 4 — Simple constraint migrations (Kind::Type; no HKT dependency beyond foundation):**
- [ ] Write `Equatable` class + instances for `Int`, `Str`, `Bool`, `Float`; remove from `satisfies_constraint` in `src/typecheck.rs` and `src/type_unify.rs` (`stdlib/prelude.llt`, `src/typecheck.rs`)
- [ ] Write `Comparable` class (extends Equatable) + instances for `Int`, `Str`, `Float`; remove from `satisfies_constraint` (`stdlib/prelude.llt`, `src/typecheck.rs`)
- [ ] Write `Showable` class + instances for `Int`, `Str`, `Bool`, `Float`, `Null`; remove from `satisfies_constraint`; `Numeric` stays hardcoded (MPTCs out of scope) (`stdlib/prelude.llt`, `src/typecheck.rs`)
- [ ] Verify prelude annotations from `builtin-type-audit` batch B still type-check after Mappable becomes a real class: `when`/`unless`, `cond`, `and`/`or`, `get-or`, `find-first`/`find-first-or`, `zip` (for both Seq×Seq and Dict×Dict); flag any annotation changes needed (`stdlib/prelude.llt`)
- [ ] Tests: `map` on user-defined Mappable type (success), `map` on non-Mappable `Int` (E010 constraint error), `AppendableSeq [Seq Int]` and `[Seq Str]` (different element types), `AppendableStr`, Equatable/Comparable/Showable constraints on user types, `satisfies_constraint` no longer special-cases any of these (`tests/corpus/eval/typecheck/`)

**Depends on:** `hkt-kind-inference`

### hkt-stdlib: Functor/Applicative/Monad/Foldable/Traversable hierarchy, Maybe, generic functions

See `doc/whatif/completed/hkt-monads.md` §The Typeclass Hierarchy, §Generic Functions. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §The Typeclass Hierarchy`, `§Generic Functions`.

All work here is stdlib declarations in `stdlib/prelude.llt`. No Rust changes needed — the type-system machinery (`Type::App`, `Kind::Operator`, class/instance registration, constraint resolution) is fully in place after `hkt-kind-inference` and `hkt-mappable-appendable`.

**Class and instance declarations (all in `stdlib/prelude.llt`):**
- [ ] Write `Functor` class (`f@Operator`, method `fmap`) + `FunctorResult`/`FunctorSeq` instances; `FunctorResult.fmap = result-map` (already in prelude) (`stdlib/prelude.llt`)
- [ ] Write `Applicative` class (extends Functor, methods `pure` + `lift2`) + `ApplicativeResult`/`ApplicativeSeq` instances (`stdlib/prelude.llt`)
- [ ] Write `Monad` class (extends Applicative, method `bind`) + `MonadResult`/`MonadSeq` instances; `MonadResult.bind = and-then` (already in prelude) (`stdlib/prelude.llt`)
- [ ] Write `Foldable` class (methods `fold`, `to-seq`) + `FoldableSeq`/`FoldableRecord`/`FoldableResult` instances; `FoldableSeq.fold = reduce`; `FoldableResult.to-seq: [fn [r] [match r [Ok a]: [a] [Err _]: []]]` — wraps `Ok` payload in singleton Seq, not bare value (`stdlib/prelude.llt`)
- [ ] Write `Traversable` class (extends Functor + Foldable, method `traverse`) + `TraversableSeq`/`TraversableResult`/`TraversableMaybe` instances; **`TraversableSeq.traverse` MUST use the primitive fold-based implementation** — NOT via generic `sequence`/`traverse` (circular): `[reduce [fn [acc x] [f.lift2 [fn [as a] [concat as [a]]] acc [f x]]] [f.pure []] xs]` (`stdlib/prelude.llt`)
- [ ] Add `Maybe` ADT (`[type [a] [Some a] [None]]`) + `FunctorMaybe`/`ApplicativeMaybe`/`MonadMaybe`/`TraversableMaybe` instances; export `Some`/`None` following `Ok`/`Err` naming pattern (`stdlib/prelude.llt`)

**Generic functions (all in `stdlib/prelude.llt`):**
- [ ] Write generic `sequence` (collapses `t (m a)` → `m (t a)` via Traversable) and `traverse` (maps then sequences); both must NOT call each other to avoid circular dependency — `traverse` is the primitive, `sequence = [fn [t] [traverse t id]]` (`stdlib/prelude.llt`)
- [ ] Write `forM` (flip-arg `traverse`), `liftM2` (via `lift2`), `when` (conditional monadic action) in `stdlib/prelude.llt` (`stdlib/prelude.llt`)

**Correctness verification:**
- [ ] Verify superclass method inheritance: each instance dict must carry ancestor methods — `MonadResult` must have `.lift2` (from Applicative) and `.fmap` (from Functor) accessible via dot access; add corpus tests for cross-superclass dispatch (`tests/corpus/eval/`)
- [ ] Verify `sequence` short-circuits: `[[Ok 1] [Err "fail"] [Ok 3]]` → `[Err "fail"]` with no evaluation of the third element; `[[Some 1] [None] [Some 3]]` → `[None]` similarly (`tests/corpus/eval/`)
- [ ] Verify `ApplicativeSeq.pure = [fn [x] [x]]` wraps `x` in a one-element Seq (not returns bare `x`) (`tests/corpus/eval/`)
- [ ] Tests: `sequence result [[Ok 1] [Err "fail"] [Ok 3]]` → `[Err "fail"]`, traverse Result and Maybe, `forM`, `when false` (body not evaluated), `liftM2`, `[do MonadMaybe ...]` with None short-circuit, `FoldableResult.to-seq [Ok 42]` → `[42]`, `FoldableResult.to-seq [Err "x"]` → `[]` (`tests/corpus/eval/`)

**Depends on:** `hkt-do-macro-explicit`, `hkt-mappable-appendable`

### hkt-doc-lsp: doc/06 Type Classes section, LSP hover, error quality

See `doc/whatif/completed/hkt-monads.md §What Would Change`. **Spec chapters:** `doc/06-type-inference.md`, `doc/whatif/completed/hkt-monads.md`.

- [x] Move `doc/whatif/hkt-monads.md` to `doc/whatif/completed/hkt-monads.md` — already done
- [x] Update `doc/whatif/index.md` Accepted section with acceptance date 2026-05-11 — already done

**Typecheck stub fix (audit finding: `src/typecheck.rs:1900` stubs `Expr::TypeApp` → `Ok(Type::Unknown)`):**
- [ ] Implement `Expr::TypeApp` in `src/typecheck.rs`: look up the resolved `App` type from the type map at the TypeApp span; if the annotation resolved to `Type::App(f, a)` during `resolve_type_expr`, the type is already in the type map — return it; emit `TypeError(E091)` if the type is not `App` (malformed TypeApp node) (`src/typecheck.rs:1900`)

**Documentation:**
- [ ] Write `§Type Classes and Higher-Kinded Types` formal rules section in `doc/06-type-inference.md`: `KIND-CLASS-PARAM` (class param annotation → `kind_env`), `KIND-OPERATOR` (App formation validation), `UNIFY-OPERATOR` / `UNIFY-APP` (already in `src/type_unify.rs`, needs spec entry), constraint generation, entailment, dictionary elaboration, parameterized instance head resolution (`doc/06-type-inference.md`)

**LSP quality:**
- [ ] Fix `Expr::TypeApp` arm in `hover_at_expr` (`src/lsp/analysis.rs`): look up the resolved type from `type_map` at the TypeApp span and display it (e.g., `[Result Int]` for `App(Result, Int)`); the current stub may return a raw annotation string rather than the resolved type — match the `Expr::Annotated` hover pattern (`src/lsp/analysis.rs`)
- [ ] Kind error message quality in `hkt-kind-inference` error sites: verify errors include annotation span, mismatched kinds, and a hint — "kind mismatch at `f`: `Int` has kind `*`, expected `* → *` — annotate as `f@Operator`"; add or improve error messages if `hkt-kind-inference` left them bare (`src/typecheck.rs`, `src/typecheck_annot.rs`)

**Verification:**
- [ ] Verify `E091` entries exist in `doc/10-errors.md` all three tables (variant catalog, codes table, categories table); add missing entries only if `hkt-kind-inference` omitted them (`doc/10-errors.md`)
- [ ] Apply stdlib prelude annotation migrations: `min`/`max`/`sorted`/`sort-by` → `fn@[return: a constraint: [a: Comparable]] [xs@Seq@a] ...`; `fold`/`reduce` → add `doc:` strings (`stdlib/prelude.llt`)
- [ ] Tests: LSP hover for `Type::App` displays resolved type (not raw annotation string); kind mismatch errors carry `[E091]` prefix and helpful hint text (`tests/lsp_corpus_tests.rs`, `tests/corpus/eval/errors/`)

**Depends on:** `hkt-stdlib`

---

## Syntax

### multi-line-strings: `unindent` stdlib function and `"""` macro

Accepted 2026-05-11. See `doc/whatif/multi-line-strings.md`. **Spec chapters:** `doc/02-syntax.md §2.3.6 Multi-Line Strings`, `doc/11-stdlib.md §Strings`. `unindent` requires no lexer changes — literal newlines in `"..."` already work. **`"""` requires lexer changes** (contradicting the whatif's claim): `lex_quoted_string` terminates at the first `"`, so `"""content"""` tokenizes as empty string + identifier + empty string; `doc/02-syntax.md §2.3.6` already documents this correctly as "requires lexer support for triple-quoted string tokens, which is not yet implemented." A parse-stage macro in `stdlib/macros.llt` cannot intercept token patterns — macros operate on AST nodes, not raw tokens.

- [x] Add `unindent` to `stdlib/prelude.llt`: use sequential fn body — binding dict `[ls: [lines s]  n: [length [last ls]]  inner: [slice 1 -1 ls]]` followed by `[join "\n" [map [fn [l] [slice n [length l] l]] inner]]`; the binding dict's entries are in scope for the final expression via `Expr::Sequential` (`stdlib/prelude.llt`)
- [ ] Add `TripleQuotedString(String)` and `TripleInterpolatedString(Vec<InterpolatedPart>)` token types to `src/lexer.rs`: detect `"""` at the start of a string context, consume content until the closing `"""`, emit accordingly; then in `src/parser.rs` desugar `TripleQuotedString(s)` → `[unindent s]` and `TripleInterpolatedString(parts)` → `[unindent i"..."]` directly in the parser (not as a stdlib macro, since macros cannot intercept token patterns) (`src/lexer.rs`, `src/parser.rs`)
- [x] Add note to `doc/02-syntax.md §String Literals` that `"..."` permits embedded literal newlines; document `"""..."""` and `i"""..."""` as the idiomatic indentation-stripping form; document `unindent` as the underlying function (`doc/02-syntax.md`)
- [x] Tests: `unindent` directly on a raw indented string, `"""..."""` value matches `[unindent "..."]`, `i"""..."""` with `$var` interpolation, single `"` inside triple-quoted content, empty lines preserved, `[trim [unindent ...]]` trailing-newline suppression (`tests/corpus/eval/`)

### multi-body-positions: Fix match arm syntax to keyed form + multi-body support

**Settled design:** Match arms use `pattern: body` keyed syntax — pattern is the KEY, body is the VALUE. Confirmed in `doc/whatif/completed/pattern-matching.md` (`n@Int: [+ n 1]`) and `doc/whatif/completed/error-patterns.md`. The current parser uses a wrong space-separated pattern-detection approach from an incorrectly implemented sprint.

Grammar: `match_form = { keyword_match ~ value ~ (pattern ~ ":" ~ value)+ }`. **Spec chapters:** `doc/02-syntax.md §3.3.4`, `doc/14-patterns.md`.

**Evaluator, desugar.rs, resolve.rs, typecheck.rs:** No changes needed — all operate on the `MatchArm` AST struct, which is syntax-agnostic.

**Parser sub-tasks** (audit finding: three distinct colon paths must each add a `StackFrame::Match` arm):
**Design decision — bracket patterns as keys: Option A decided (skeptic + CS, 2026-05-12).** Option B is not viable: `push_expr_to_parent` immediately converts expressions to `Pattern` via `expr_to_pattern_with_guard`; by the time `:` fires the expression is already a Pattern with no Expr to pop. Option A defers pattern conversion: add `pending_pattern_expr: Option<Spanned<Expr>>` as a staging slot; store raw Expr there on arrival; convert to Pattern only when the colon confirms it. `scrutinee.is_some()` distinguishes scrutinee from first pattern — no `has_scrutinee` flag needed. Nested brackets work automatically via the existing stack machinery.

- [ ] Add `pending_pattern_expr: Option<Spanned<Expr>>` field to `StackFrame::Match` (`src/parser.rs:999`): staging slot for a bracket expression that may be a pattern, awaiting colon confirmation before `expr_to_pattern_with_guard` is called
- [ ] Modify `push_expr_to_parent` Match arm (`src/parser.rs:4500–4508`): when `scrutinee.is_some() && pending_pattern.is_none() && pending_pattern_expr.is_none()`, store incoming expr in `pending_pattern_expr` instead of immediately calling `expr_to_pattern_with_guard`; the colon now triggers conversion, not arrival (`src/parser.rs`)
- [ ] Add `StackFrame::Match` arm to `Token::Colon` handler (`src/parser.rs:2557`): when colon fires with `pending_pattern_expr.is_some()`, call `expr_to_pattern_with_guard`, store result as `pending_pattern`; error if `pending_pattern_expr.is_none()` ("`:` without a pattern in match form") (`src/parser.rs`)
- [ ] Add `StackFrame::Match` arm to identifier-with-colon-ahead detection (`src/parser.rs:2871–2941`): store bare identifier in `pending_pattern_expr` (parallel to `StackFrame::Dict` arm that sets `pending_key`) (`src/parser.rs`)
- [ ] Add `StackFrame::Match` arm to annotated-expr-with-colon detection (`src/parser.rs:2813–2823`): store annotated expression in `pending_pattern_expr` to support `n@Int:` arm syntax (`src/parser.rs`)
- [ ] Add orphan check to `StackFrame::Match` CloseBracket handler (`src/parser.rs:2386`): if `pending_pattern_expr.is_some()` at close time, error "match pattern must be followed by `:` and a body" (`src/parser.rs`)
- [x] **Bracket-pattern-as-key design decision** — resolved: Option A with `pending_pattern_expr` staging slot (`src/parser.rs`)
- [ ] Multi-body match arms: allow the VALUE side of a `pattern:` entry to be `Expr::Sequential` — the parser wraps multiple body expressions in Sequential when the value is a dict-like block; no new parser arms needed (`src/parser.rs`)
- [ ] Multi-body match arms: allow the VALUE side of a `pattern:` entry to be `Expr::Sequential` — the parser wraps multiple body expressions in Sequential when the value is a dict-like block; no new parser arms needed (`src/parser.rs`)

**Formatter:**
- [ ] Update `src/formatter.rs` match arm formatting: output `pattern: body` (not `pattern body`); align `:` across all arms in a match form; when body is `Expr::Sequential`, format its expressions indented on separate lines (`src/formatter.rs`)

**Corpus tests (~36 files, all use old space-separated syntax):**
- [ ] Rewrite all `tests/corpus/eval/match_*.llt-eval` files to keyed `pattern: body` syntax (10 files: `match_variable_binding`, `match_type_int`, `match_type_str`, `match_type_number`, `match_wildcard`, `match_literal_int`, `match_literal_str`, `match_literal_bool`, `match_dict_type`, `match_nested`) (`tests/corpus/eval/`)
- [ ] Rewrite all `tests/corpus/eval/pattern_matching/*.llt-eval` files to keyed syntax (~20 files: guard tests, dict destructure tests, seq tests, open/closed matching tests) (`tests/corpus/eval/pattern_matching/`)
- [ ] Rewrite `tests/corpus/eval/match/pin_pattern.llt-eval` and BAS typecheck match tests (`bas_i_case3_three_arms`, `bas_cls_bot`, `rdnf_match_union_simplification`, `match_arm_scope`) to keyed syntax (`tests/corpus/eval/`)
- [ ] Rewrite `tests/corpus/eval/stdlib/` match-containing tests to keyed syntax (`ok_ctor_no_circular`, `try_result_match_ok`, `try_result_match_err`, `toml_lite_array_*`) (`tests/corpus/eval/stdlib/`)
- [ ] Fix `tests/cli_tests.rs:2152` inline `[match]` expression to keyed syntax (`tests/cli_tests.rs`)

- [x] Extend `[defmacro ...]` body parsing in `src/parser.rs` — already implemented at `src/parser.rs:2360–2369`
- [x] Update `doc/02-syntax.md §3.3.4` grammar rule and examples to keyed `pattern: body` syntax — done 2026-05-12
- [x] Update `doc/14-patterns.md` to keyed `pattern: body` syntax — done 2026-05-12
- [x] Update all doc/*.md and doc/feature/*.md match examples to keyed syntax — done 2026-05-12

---

## Capability System

### dir-cap-permissions: Fine-grained read/write/list permissions on DirCap and cap-file

See `doc/whatif/dir-cap-permissions.md` (Accepted 2026-05-11). Extends `--cap-fs` (and `--cap-file`) with an optional `:MODE` suffix using letter bundles and an extended `:[Cap1 Cap2 ...]` list syntax; adds a `DirPerms` bitfield to `Value::DirCap`; enforces permissions in DirCap-consuming builtins. No mode on either flag = full access (all capabilities). **Spec chapters:** `doc/whatif/dir-cap-permissions.md`.

**Type system encoding under BAS:** The whatif describes `DirCap[Writable ...]` with a row tail — that was Rémy-style row polymorphism, which BAS removed. The correct BAS encoding is **intersection types**: `DirCap[Writable ...]` → `@[[all DirCap Writable]]` = `Type::Intersection([DirCap, Writable])`. The "at least these flags" semantics fall out from BAS intersection elimination: `A & B & C <: A & B` (if you have all three, you have any two), so `@[[all DirCap Readable Writable]] <: @[[all DirCap Writable]]` — a fully-capable DirCap satisfies any subset-capability constraint.

Capability flags (`Readable`, `Writable`, `Listable`, `Statable`, `Appendable`, `Deletable`, `Renameable`) are registered as nominal unit types in `TypeEnv`. Annotation `@DirCap[Writable]` in `caps:` dicts desugars to `@[[all DirCap Writable]]`.

**Mode grammar (same for `--cap-fs` and `--cap-file`):**
- No `:mode` suffix → full access (all applicable capabilities)
- Letter sequence: each letter adds its bundle — `r` = `{Readable, Listable, Statable}`, `w` = `{Writable, Appendable, Deletable, Renameable}`, `a` = `{Appendable}`, `s` = `{Statable}`, `l` = `{Listable, Statable}`; letters compose by union (`rw` = r∪w)
- Extended syntax: `:[Cap1 Cap2 ...]` — parse as whitespace-separated capability names, exact set granted, no implied additions; detected by mode starting with `[`
- For `--cap-file`: additional `Binary` flag in extended syntax (`:[Readable Binary]`); letter shorthands `r`/`rb`/`w`/`wb` remain as before (backward compat)

- [x] Refactor `--cap-fs` argument parsing in `src/main.rs`: split on last `:` via `rsplit_once`; if no `:` present, grant full `DirPerms::full()`; if mode starts with `[`, parse as extended capability list; otherwise parse letter-by-letter accumulating bundles (`r`→Readable+Listable+Statable, `w`→Writable+Appendable+Deletable+Renameable, `a`→Appendable, `s`→Statable, `l`→Listable+Statable); unknown letter = startup error (`src/main.rs`)
- [ ] Extend `--cap-file` argument parsing in `src/main.rs`: same extended syntax — if mode starts with `[`, parse as `[Cap1 Cap2 ...]` list (valid names: `Readable`, `Writable`, `Appendable`, `Binary`); no `:mode` suffix → open file read-write (equivalent to `rw`); retain existing `r`/`rb`/`w`/`wb` letter shorthands for backward compat (`src/main.rs`)
- [x] Add `DirPerms { readable, statable, listable, writable, appendable, deletable, renameable: bool }` struct to `src/value.rs`; add `perms: DirPerms` field to `Value::DirCap` and `Value::RevocableDirCap`; update all construction sites to use `DirPerms::full()` (`src/value.rs`)
- [ ] Implement `open` write and append paths in `builtin_open`: the `Writable` and `Appendable` flag branches currently return "not yet implemented" (`src/builtins_io.rs:197,371`); implement using `dir.open_with(path, OpenOptions::new().write(true).create(true).truncate(true))` for Writable and `.append(true)` for Appendable; wrap result in `Value::WriteHandle` with appropriate caps (`src/builtins_io.rs`)
- [x] Enforce permissions in `builtin_open`: `readable` for `"r"`, `writable` for `"w"`, `appendable` for `"a"`; capability error `"DirCap: open requires <Readable|Writable|Appendable> permission"` on violation (`src/builtins_io.rs`)
- [x] Enforce `listable` in `builtin_list_dir`; enforce `writable` in `builtin_write`/`builtin_write_atomic`; stubs for future `builtin_delete_file` (needs `deletable`) and `builtin_rename_file` (needs `renameable`) (`src/builtins_io.rs`)
- [ ] Register capability flag nominal unit types in `TypeEnv`: `Readable`, `Writable`, `Listable`, `Statable`, `Appendable`, `Deletable`, `Renameable` — each as a singleton `Type::NominalTag` (no payload); register `%pwd` and `--cap-fs` DirCaps using BAS intersection types; update builtin type signatures using `Type::Intersection([DirCap, Flag])`: `list-dir` → `Intersection([DirCap, Listable])`, `open "r"` → `Intersection([DirCap, Readable])`, `open "w"` → `Intersection([DirCap, Writable])`; the subtyping `Intersection([DirCap, Readable, Writable]) <: Intersection([DirCap, Writable])` holds automatically under BAS intersection elimination (`src/type_env.rs`, `src/types.rs`)
- [ ] Add `narrow` overload for DirCap: `[narrow cap@[[all DirCap Flag1 ...]] FlagName...]` produces a new DirCap with the intersection of source permissions and requested flags; the return type is `Intersection([DirCap, requested-flags])` — a BAS intersection narrower than the input; runtime error if a requested flag is not held in the source `DirPerms`; `[narrow cap Subtree "path"]` restricts the directory root to a subdirectory and returns the same intersection type with an updated root path (`src/builtins_io.rs` or new `src/builtins_cap.rs`)
- [ ] Tests: `--cap-fs root=.:r` → `list-dir` succeeds, `open "w"` fails; `--cap-fs data='./d:[Readable Statable]'` → read succeeds, `list-dir` fails; `--cap-file cfg=Cargo.toml` (no mode) → read-write handle; extended syntax `--cap-file cfg='Cargo.toml:[Readable]'` → read-only handle; `narrow` reduces permissions; `narrow` to non-held flag errors (`tests/corpus/eval/`, `tests/corpus/cli/`)

---

## Standard Library Boundary

### stdlib-tinct-migration: Move redundant Rust builtins to native tinct

Findings from the builtin boundary audit (2026-05-13). These Rust builtins are unnecessary — they can be expressed entirely using existing primitives with no new Rust code.

- [ ] Replace `record?` Rust builtin with a tinct alias in `stdlib/prelude.llt`: `record?: dict?` — at runtime `record?` and `dict?` are identical; the distinction is type-level only, already handled by the type checker (`stdlib/prelude.llt`, `src/builtins_meta.rs`)
- [ ] Replace `map?` Rust builtin with a tinct alias in `stdlib/prelude.llt`: `map?: dict?` — same reasoning as `record?`; remove both from `standard_builtins()` and `TypeEnv::with_builtins()` after the tinct aliases are verified (`stdlib/prelude.llt`, `src/builtins_meta.rs`, `src/type_env.rs`)
- [ ] Replace `num?` Rust builtin with a tinct definition in `stdlib/prelude.llt`: `num?: [fn [x] [or [int? x] [float? x]]]` using existing `int?`, `float?`, `or`; remove from `standard_builtins()` (`stdlib/prelude.llt`, `src/builtins_meta.rs`)

### stdlib-io-tinct-migration: Move I/O builtins that don't need to be in Rust

Audit findings (2026-05-13): most I/O builtins genuinely require Rust (28 irreducible syscall/opaque-type primitives). These specific ones do not.

- [ ] Move `spki-pin` to tinct in `stdlib/net.llt`: pure dict construction, no syscalls, no Rust crates; `[fn [algorithm fingerprint] [if [not [has? valid-algos algorithm]] [error [str "unknown algorithm: " algorithm]] [set [set [] "algorithm" algorithm] "fingerprint" fingerprint]]]` where `valid-algos` is a tinct dict of accepted names (`stdlib/net.llt`, `src/builtins_io.rs`)
- [ ] Add `raw-create : DirCap → Str → WriteHandle` Rust primitive: opens a file for writing (create/truncate) returning a `WriteHandle`; this splits the current `write(DirCap, path, String)` path to allow tinct-level pipe construction (`src/builtins_io.rs`, `src/type_env.rs`)
- [ ] Once `raw-create` lands, rewrite `copy` in tinct: `[fn [cap src dst] [close [write-handle [raw-create cap dst] [slurp [open cap src Readable Text]]]]]`; remove `copy` Rust builtin from `standard_builtins()` (`stdlib/io.llt`, `src/builtins_io.rs`)
- [ ] Change `cap-data` to return `Null` (empty dict `[]`) when the capability name is not present instead of erroring — this makes it a proper nullable lookup compatible with `get-or` and `has?` patterns (`src/builtins_io.rs`)
- [ ] Once `cap-data` returns null on miss, rewrite `has-cap?` in tinct as `[fn [h cap] [not [null? [cap-data h cap]]]]`; remove `has-cap?` Rust builtin (`stdlib/io.llt`, `src/builtins_io.rs`)
- [ ] Investigate and remove the vestigial Rust `http-get` builtin (the `HttpConn` form using `Value::HttpConn`/reqwest client directly): verify it is not called by any corpus tests, `stdlib/net.llt`, or user-facing code; `net.llt`'s `http-get` already implements HTTP without it (`src/builtins_io.rs`, `src/type_env.rs`)

### stdlib-new-primitives: Add missing Rust primitives to unlock tinct migrations

Five new Rust primitives identified by the boundary audit as the highest-leverage additions for shrinking the Rust surface area. Each unlocks one or more stdlib functions that can move from Rust to tinct.

- [ ] Add `str-index-of : Str → Str → Int` Rust builtin: native O(n) substring search returning start byte-index or -1 on miss; wraps `str::find`; replaces the O(n²) `str-find-impl` in prelude with an O(n) call; unlocks `str-contains?`, `starts-with?`, `ends-with?` as tinct wrappers around this primitive (`src/builtins_string.rs`, `src/type_env.rs`)
- [ ] Once `str-index-of` lands, rewrite `str-contains?`, `starts-with?` (string form), `ends-with?` (string form) in `stdlib/strings.llt` as tinct wrappers; remove the three Rust builtins from `standard_builtins()` (`stdlib/strings.llt`, `src/builtins_string.rs`)
- [ ] Add `str-map-chars : (Str → Str) → Str → Str` Rust builtin: map a tinct function over Unicode codepoints, returning a new string; unlocks `upper`, `lower`, and character-level transforms as tinct stdlib functions (`src/builtins_string.rs`, `src/type_env.rs`)
- [ ] Once `str-map-chars` lands, rewrite `upper` and `lower` in `stdlib/strings.llt` as tinct functions using `str-map-chars` + `char-code`/`chr` arithmetic for ASCII fast path; remove Rust builtins (`stdlib/strings.llt`, `src/builtins_string.rs`)
- [ ] Add `trim-start : Str → Str` and `trim-end : Str → Str` Rust builtins (complement to existing `trim`): strip leading/trailing whitespace from one end only; these enable richer string normalization in tinct stdlib without needing `str-map-chars` (`src/builtins_string.rs`, `src/type_env.rs`)
- [ ] Add `regex-match? : Str → Str → Bool` Rust builtin: test if a regex pattern matches anywhere in a string using the `regex` crate; unlocks the `pattern` constraint in `validate` and other regex-dependent stdlib functions as tinct code (`src/builtins_string.rs`, `src/type_env.rs`)
- [ ] Once `regex-match?` lands, rewrite `validate`'s `pattern` constraint check in tinct; identify which other parts of `validate` can move to tinct vs what must remain Rust (`stdlib/prelude.llt` or `src/builtins_meta.rs`)
- [ ] Make `builtin-sort` accept an optional comparator argument: `builtin-sort : ((a → a → Bool)? → Dict → Dict)`; when provided, use comparator instead of natural type ordering; this allows `sort` and `sort-by` in prelude to both reduce to one Rust primitive (`src/builtins_seq_prim.rs`, `src/type_env.rs`)

## Internal Integrity

### primitive-privacy: Hide all Rust primitives from user code behind prelude.llt

**Goal:** User code sees only names exported by `prelude.llt`. Raw Rust primitives (the entire `standard_builtins()` registry) are invisible to user code. No backwards-compatibility concern — tinct has no stable public API yet.

**Design (from `doc/whatif/builtin-privacy.md` Approach A, extended to full primitive set):**

```
prelude_eval_env = create_root_env() + inject_prelude_aliases()
  (prelude.llt evaluates here, can see all Rust builtins + builtin-* aliases)
  ↓
prelude_output_env = result of evaluating prelude.llt
  (only names prelude.llt defines — no Rust builtins leak through)
  ↓
user env
  (only sees prelude exports)
```

`prelude.llt` is responsible for explicitly exporting every name it wants users to have access to. For Rust builtins with no tinct wrapper (e.g., `emit`, `error`, `type-of`, `from-json`), prelude.llt adds a pass-through: `emit: builtin-emit` or defines a proper wrapper. The builtin-audit sprint (below) identifies what's missing.

**Already done:**
- [x] Migrate `stdlib/macros.llt`, `stdlib/path.llt`, `stdlib/toml-lite.llt` to prelude wrappers — no `builtin-*` calls remain
- [x] Split `create_root_env()` / `inject_prelude_aliases()` in `src/builtins.rs`
- [x] Type-checker warning `T009` for `builtin-*` references outside `prelude.llt`

**Remaining (env isolation — gate on `stdlib-primitive-audit`):**
- [ ] **[THE SWITCH]** Update prelude loading in `src/imports.rs` (`build_prelude_env`): build `prelude_eval_env` = `create_root_env()` + `inject_prelude_aliases()`; evaluate `prelude.llt` in `prelude_eval_env`; the output of prelude evaluation becomes the parent of the user env — user env does NOT inherit `create_root_env()` directly; after this change, any builtin not re-exported by prelude or stdlib becomes an `undefined variable` error in user code (`src/imports.rs`, `src/builtins.rs`)
- [ ] Add all user-facing Rust primitives that lack prelude wrappers as explicit pass-throughs or wrappers in `stdlib/prelude.llt` (see `stdlib-primitive-audit` sprint for the complete list) — gate on audit results (`stdlib/prelude.llt`)

**Depends on:** `stdlib-primitive-audit`
- [x] Remove vestigial Rust `http-get` builtin (the `Value::HttpConn`/reqwest form) — done 2026-05-13: deleted `builtin_http_get` (107 lines), removed `Value::HttpConn` variant, removed from `standard_builtins()` and `TypeEnv`, removed from `builtins_meta.rs` and `lib.rs` JSON serialization (`src/builtins_io.rs`, `src/builtins.rs`, `src/type_env.rs`, `src/value.rs`)
- [ ] Tests: user code referencing a raw builtin name → `undefined variable` error (pick 3–5 builtins not re-exported by prelude as test cases); `prelude.llt` itself still works; corpus tests still pass (`tests/corpus/eval/`)

### stdlib-primitive-audit: Add all missing raw-primitive re-exports to prelude.llt

**Audit complete (2026-05-13).** Of ~180 registered builtins, ~130 have no prelude.llt wrapper.

**Key architectural point:** `prelude.llt` is the single choke point. When a user does `[include %libdir "net.llt"]`, net.llt is evaluated in the user env — which after the switch only has prelude exports. So if `connect` and `tls-layer` aren't in prelude, net.llt gets `undefined variable: connect`. Every primitive that any stdlib file needs must be re-exported by prelude first. The stdlib modules (io.llt, net.llt, datetime.llt) build higher-level tinct APIs on top of what prelude exposes — they do not independently wrap Rust primitives.

**All missing re-exports go in `stdlib/prelude.llt`**, grouped by domain for readability. Each is a simple pass-through (`name: name`) unless a wrapper adds value.

- [ ] General string ops: re-export `str`, `split`, `replace`, `trim`, `upper`, `lower`, `starts-with?`, `ends-with?`, `str-chars`, `str-length`, `str-slice`, `str-contains?` (`stdlib/prelude.llt`)
- [ ] Collection ops: re-export `keys`, `length`, `merge`, `append`, `each`, `each-key`, `each-kv` (`stdlib/prelude.llt`)
- [ ] Numeric/type: re-export `floor`, `round`, `to-int`, `to-float`, `float`, `type-of` (`stdlib/prelude.llt`)
- [ ] Type predicates: re-export `int?`, `float?`, `num?`, `str?`, `bool?`, `null?`, `dict?`, `fn?`, `seq?`, `bytes?`, `record?`, `map?` — audit found these are NOT in prelude's public dict despite being used throughout (`stdlib/prelude.llt`)
- [ ] Error/control: verify `error`, `try`, `eval`, `apply`, `force`, `until` are explicitly in the public dict (they're used in prelude but may not be re-exported as public names) (`stdlib/prelude.llt`)
- [ ] Data: re-export `from-json`, `validate`, `emit`, `env` (`stdlib/prelude.llt`)
- [ ] Bytes/encoding: re-export `bytes`, `bytes-find`, `bytes-of`, `bytes-equal?`, `ct-equal?`, `str-bytes`, `bytes-str`, `char-code`, `chr` — needed by encoding.llt which is loaded at startup (`stdlib/prelude.llt`)
- [ ] Math primitives: re-export `pow`, `sqrt`, `log`, `log2`, `log10`, `exp`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `nan?`, `inf?`, `finite?`, `band`, `bor`, `bxor`, `shl`, `shr` — needed by math.llt which is loaded at startup (`stdlib/prelude.llt`)
- [ ] I/O primitives: re-export `open`, `slurp`, `lines`, `write`, `write-atomic`, `write-handle`, `flush`, `close`, `seek`, `seek-end`, `position`, `list-dir`, `stat`, `make-dir`, `remove`, `rename`, `link`, `read-link`, `narrow`, `revocable`, `revoke-cap`, `cap-data`, `has-cap?` — needed by io.llt (`stdlib/prelude.llt`)
- [ ] Network primitives: re-export `connect`, `tls-layer`, `tls-peer-cert`, `spki-pin`, `send-datagram`, `recv-datagram`, `http-request`, `http2-session`, `http3-session`, `quic-session`, `quic-open-stream`, `quic-open-datagram`, `icmp-ping`, `uri`, `url`, `urn` — needed by net.llt (`stdlib/prelude.llt`)
- [ ] Date-time primitives: re-export all timestamp/duration/timezone ops (`parse-timestamp`, `format-timestamp`, `timestamp->unix`, `unix->timestamp`, `now`, `fixed-clock`, `timestamp-add`, `timestamp-diff`, `timestamp<?`/`>`/`=?`, `timestamp-year`…`timestamp-second`, `timestamp-parts`, `duration-nanos`…`duration->nanos`, `load-tz`, `timestamp-in-tz`, `local->timestamp`, `local-tz-name`) — needed by datetime.llt (`stdlib/prelude.llt`)

**Intentionally NOT re-exported (internal or meta-language):**
- `eval-ast`, `gensym` — macro infrastructure internal to `src/expand.rs`
- `llt-repr`, `tag-of`, `variant` — debugging/introspection; document as internal
- `decimal`, `big-int` — no dedicated stdlib module yet; leave raw until numeric.llt exists
- `proxy` — advanced metaprogramming; leave raw
- `include` — special form consumed by parser, not a callable function

---

## Networking

### net-gaps: QUIC datagrams, SPKI correctness, HTTP/3 concurrent driver

Genuine deferred items from the `http-sessions` and `connector-tls` sprints. Each is a deliberate "implement later" stub.

- [x] Remove `socks5-connect` and `proxy-connect` from `standard_builtins()`, `TypeEnv::with_builtins()`, and the builtin count assertion — decided 2026-05-09 to remove from registry (they return "not yet implemented" errors and SOCKS5 is implemented as a pure-tinct `socks5-layer` in stdlib) (`src/builtins.rs`, `src/builtins_io.rs`, `src/type_env.rs`) — ALREADY DONE (verified 2026-05-11: not present in any of these files)
- [x] Delete stale SPKI comment at `src/builtins_io.rs:3335` — two lines saying "simplified implementation that hashes the whole cert"; `compute_spki_hash` already correctly extracts `subject_pki.raw` (`src/builtins_io.rs`) — ALREADY DONE (verified 2026-05-11: comment not present, implementation correct at line 3121)
- [x] Add `Value::QuicDatagramHandle(Rc<quinn::Connection>)` variant to the `Value` enum and its `type_name`/`Display`/`PartialEq` impls (`src/value.rs`) — ALREADY DONE (line 396 + impls at lines 474, 560, 648, 735)
- [x] Register `Type::QuicDatagramHandle` in `TypeEnv::with_builtins` and add type signature for `quic-open-datagram` (`src/type_env.rs`) — ALREADY DONE (lines 1886-1891)
- [x] Implement `quic-open-datagram`: replace the current "not yet implemented" error body with `block_on(session.open_uni())` to get a send stream; return `Value::QuicDatagramHandle(Rc::clone(&conn))` (`src/builtins_io.rs:4457`) — ALREADY DONE (lines 4181-4231: returns Value::QuicDatagramHandle(conn))
- [ ] Add `send-datagram` overload for `Value::QuicDatagramHandle`: dispatch to `block_on(conn.send_datagram(bytes))` (`src/builtins_io.rs`)
- [ ] Add `recv-datagram` overload for `Value::QuicDatagramHandle`: dispatch to `block_on(conn.read_datagram())`, return `Bytes` (`src/builtins_io.rs`)
- [ ] Add `async_rt::spawn<F: Future>(fut: F) -> JoinHandle<F::Output>` helper using `TOKIO_RT.with(|rt| rt.spawn(fut))` — tokio `current_thread` runtime drives spawned tasks during `block_on` calls (`src/async_rt.rs`)
- [ ] Define `Http3SessionState { send_request: h3::client::SendRequest<...>, _driver: JoinHandle<()> }` struct in `src/builtins_io.rs`; spawn the h3 `Connection` driver via `async_rt::spawn` and store its `JoinHandle` in the struct to keep it alive
- [ ] Change `Value::Http3Session` to wrap `Rc<RefCell<Http3SessionState>>` instead of the bare `send_request`; update all match arms that destructure it (`src/value.rs`, `src/builtins_io.rs`)
- [ ] Tests: `quic-open-datagram` + `send-datagram` + `recv-datagram` round-trip corpus test; `http3-session` concurrent request (two sequential requests on one session succeed); QUIC datagram type error on wrong handle type (`tests/corpus/eval/`)

---

## LSP

### lsp-gaps: Prelude go-to-definition and remaining LSP quality items

- [x] **Prelude go-to-definition** (`src/lsp/analysis.rs:802`): Parse the embedded prelude source (`include_str!("../../stdlib/prelude.llt")`) once at LSP startup into a `Spanned<File>` AST and cache it in `DocumentStore`; extend `definition_at()` in `src/lsp/analysis.rs` to search the cached prelude AST using the existing `find_key_definition()` recursion after local/include lookup fails; resolve the prelude URI via `find_libdir_path().join("prelude.llt")` + `file_path_to_uri()` for the `Location` response; `llt_span_to_lsp_range` works unchanged since it takes source text separately from spans (`src/lsp/analysis.rs`, `src/lsp/document.rs`)
- [ ] **`textDocument/documentSymbol`:** walk the top-level dict entries of the current document and return them as `SymbolKind::Variable` symbols with their definition spans; add `document_symbols_at` in `src/lsp/analysis.rs`; register `DocumentSymbolRequest::METHOD` in `src/lsp/server.rs`; declare capability in `ServerCapabilities`; enables IDE outline views and breadcrumbs (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`textDocument/formatting`:** call the existing Rust formatter (`src/formatter.rs`) on the full document source and return a single whole-document `TextEdit`; register `DocumentFormattingRequest::METHOD` in `src/lsp/server.rs`; declare `document_formatting_provider` in `ServerCapabilities`; the formatter already produces a round-tripped source string — wrap it in a diff against the original to produce minimal edits, or return a single replace-all edit for simplicity (`src/lsp/server.rs`, `src/formatter.rs`)
- [ ] **`textDocument/references`:** find all spans in the document where a given name is referenced; add `references_at(doc, offset) -> Vec<Location>` in `src/lsp/analysis.rs` — walk the full AST collecting all `Expr::VarRef` nodes whose name matches the symbol under the cursor; register `References::METHOD` in `src/lsp/server.rs`; declare `references_provider` in `ServerCapabilities` (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`textDocument/rename`:** rename a binding and all its references in the document; reuse `references_at` plus the definition span to produce a `WorkspaceEdit` with `TextEdit` entries for every occurrence; validate the new name is a valid tinct identifier before returning; register `Rename::METHOD` in `src/lsp/server.rs`; declare `rename_provider` in `ServerCapabilities` (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`textDocument/inlayHints`:** return inferred types inline next to unannotated bindings in the visible range; add `inlay_hints_in_range(doc, range) -> Vec<InlayHint>` in `src/lsp/analysis.rs` — for each top-level dict entry whose value is not annotated, look up its inferred `TypeScheme` from the type map and emit a hint with the display string (e.g., `: Int`, `: Fn@Bool [a a]`) positioned after the binding name; register `InlayHintRequest::METHOD` in `src/lsp/server.rs`; declare `inlay_hint_provider` in `ServerCapabilities`; this is the highest-information-density feature for a type-inferred language (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`textDocument/signatureHelp`:** when the cursor is inside a function call `[f ...]`, look up `f`'s `TypeScheme`, extract parameter names and types, and return a `SignatureInformation` showing the full `Fn@Return [param1@Type ...]` signature with the active parameter highlighted based on cursor position; register `SignatureHelpRequest::METHOD` in `src/lsp/server.rs`; declare `signature_help_provider` in `ServerCapabilities` (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`workspace/symbol`:** search all top-level bindings across all open and recently-loaded documents matching a query string; return as `WorkspaceSymbol` entries with their file URIs and definition ranges; register `WorkspaceSymbolRequest::METHOD` in `src/lsp/server.rs`; declare `workspace_symbol_provider` in `ServerCapabilities` (`src/lsp/server.rs`, `src/lsp/document.rs`)
- [ ] **Hover: show inferred type alongside declared annotation when they differ** (`src/lsp/analysis.rs`): when a binding has an explicit annotation and the inferred type from `type_map`/`scheme_map` is strictly narrower (e.g., declared `@Number` but inferred `Int`, declared `@Dict` but inferred `{name: String}`), append the inferred type to the hover: `"x (Number) — inferred: Int"`; use `is_subtype(inferred, declared) && !is_subtype(declared, inferred)` to detect the "narrower" case; no change needed when they match or when there is no annotation (`src/lsp/analysis.rs`, `src/types.rs`)
- [x] [Major] Verify LSP `document.rs` `update_document` calls `desugar_file()` BEFORE `typecheck_file()` — all other entry points follow `expand_macros → desugar → resolve → typecheck → eval`; if LSP reorders or skips desugar, the type checker sees `VarRef("_")` instead of desugared `Fn` nodes producing spurious "undefined variable _" errors; confirm and add a PIPELINE INVARIANT comment (`src/lsp/document.rs`)

---

## Evaluator and Macros

### eval-gaps: Unquote nesting, error span threading

Two correctness/quality gaps in the evaluator noted in source comments.

- [ ] **Unquote in nested positions** (`src/eval.rs:1343`): The `eval_quote` fallback arm (`_ =>`) calls `ast_to_dict_expr` which does not recognize `Expr::Unquote`/`Expr::UnquoteSplice` in nested positions; add a recursive `eval_quote_expr` pre-pass in `src/eval.rs` that walks the full `Expr` tree — when it encounters `Expr::Unquote(inner)`, evaluate `inner` and substitute the result as a serialized AST value node; when it encounters `Expr::UnquoteSplice(inner)` in a list position, splice the evaluated sequence; all other nodes recurse unchanged; replace the `_ =>` arm with a call to `eval_quote_expr` then `ast_to_dict_expr`; `ast_to_dict_expr` is unchanged (`src/eval.rs`); add corpus tests for nested `[unquote ...]` inside call args, dict values, and seq literals (`tests/corpus/eval/`)
- [x] Remove stale `#[allow(dead_code)]` attribute on `eagerly_register_constructors` in `src/eval.rs:1261` — the function is actively called from `src/eval_dict.rs` and the lint fires spuriously for `pub(crate)` items in some configurations (`src/eval.rs`)
- [x] Fix stale test comment at `src/typecheck.rs:8200` that says "`@[...]` composite annotation is not yet implemented in the parser" — `Annotation::PropertyDict` is fully implemented and used throughout the prelude; update the comment (`src/typecheck.rs`)
- [x] Make TypeAssert materialization iterative: replace the `eval_recursive` call at `src/eval_materialize.rs:1655` (`TODO(cek-eval)`) with a `TypeAssertCheck` continuation — push the check onto the continuation stack and use `Action::Eval` for the inner expression instead of recursing (`src/eval_materialize.rs`)
- [x] **`mat_span` threading through DotAccessForceData** (`src/eval_materialize.rs:1344`, `src/eval_materialize.rs:1379`): When `.field` access in an access chain triggers materialization, the `mat_span` used is the access expression span rather than the outer materialization context's span — this loses the outermost call-site span in error messages for chained access like `a.b.c`; fix by threading `outer_mat_span: Option<Span>` through `DotAccessForceData` and using it in `Action::Materialize`; corresponding test is at `src/eval.rs:5559` (currently asserts the wrong span as a known limitation — update when fixed)

---

## Tooling

### tinct-hosted-formatter: Implement stdlib/formatter/format.llt

Accepted 2026-05-05. See `doc/whatif/completed/tinct-hosted-formatter.md` for the full design.
The Rust formatter (`src/formatter.rs`) is retained for LSP use; this formatter receives the AST dict from `ast_to_dict` and returns formatted source as a tinct string.

- [ ] Implement `stdlib/formatter/compact.llt` and `stdlib/formatter/pretty.llt` as tinct programs that receive `%` as the AST dict (from `ast_to_dict(Some(src), Some(comments))`) and return formatted source; wire `tinct fmt --compact`/`--pretty` to invoke these via the evaluator
- [ ] Implement `stdlib/formatter/format.llt` as the full formatter — layout algorithm, indentation, comment attachment, multi-line decisions per `doc/whatif/completed/tinct-hosted-formatter.md`; wire to `tinct fmt` (default mode)
- [ ] The Rust formatter (`src/formatter.rs`) is retained for LSP use — add a `FormatterMode` enum to dispatch between Rust and tinct-hosted based on invocation context; LSP always uses Rust formatter
- [ ] Tests: round-trip corpus tests (format → re-parse → compare AST); test compact/pretty/full modes; test comment preservation

