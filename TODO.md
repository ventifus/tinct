# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Research (requires /rnd before implementing)

- [ ] Research runtime reflection — see `doc/whatif/runtime-reflection.md`. Design: `Value::Function` carries `FnAnnotation` metadata at runtime; `describe` / `render` / `module-docs` / `annotation-of` / `source-of` builtins; `render` replaces manual AST-dict walking in tinct-hosted formatter and makes value→source round-trip possible; enables REPL `:describe`, programmatic docgen via `module-docs`, and metaprogramming.

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
- [x] Write `Mappable` class + `MappableSeq`/`MappableRecord` instances in `stdlib/prelude.llt`; `f@Operator` on the class param now works after hkt-kind-inference (`stdlib/prelude.llt`)
- [x] Remove hardcoded `Mappable` placeholder `ClassDecl` from `InferState::new()` in `src/types.rs` — the class is now declared in prelude and registered via normal class-loading; also remove `Mappable` from the `satisfies_constraint` hardcoded match in `src/typecheck.rs` only after end-to-end verification that `map` on a user-defined `Mappable` type works (`src/types.rs`, `src/typecheck.rs`)
- [ ] Update `$map`/`$filter` type signatures in `src/type_env.rs` to use `Mappable f` constraint instead of hardcoded dual-dispatch (`src/type_env.rs`)

**Phase 3 — Appendable migration (Kind::Type; simpler, no Operator dependency):**
- [x] Write `Appendable` class (kind-`*`) + `AppendableStr`/`AppendableRecord` instances + parameterized `AppendableSeq [Seq b]` instance (relies on resolve_instance freshening) in `stdlib/prelude.llt` (`stdlib/prelude.llt`)
- [x] Remove `Appendable` from `satisfies_constraint` hardcoded match; update `$concat`/`$conj` type sigs in `src/type_env.rs` to use `Appendable a` (`src/typecheck.rs`, `src/type_env.rs`)

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

## Standard Library Boundary

### stdlib-boundary: stdlib Rust surface area reduction

Audit findings (2026-05-13): most I/O builtins genuinely require Rust (28 irreducible syscall/opaque-type primitives). These specific ones do not. Also adds missing Rust primitives that unlock tinct migrations, and verifies all stdlib modules use `%rust` groups cleanly after `primitive-privacy` Phase 3 lands.

**Depends on:** `stdlib-tinct-migration`

- [ ] Move `spki-pin` to tinct in `stdlib/net.llt`: pure dict construction, no syscalls, no Rust crates; `[fn [algorithm fingerprint] [if [not [has? valid-algos algorithm]] [error [str "unknown algorithm: " algorithm]] [set [set [] "algorithm" algorithm] "fingerprint" fingerprint]]]` where `valid-algos` is a tinct dict of accepted names (`stdlib/net.llt`, `src/builtins_io.rs`)
- [ ] Add `raw-create : DirCap → Str → WriteHandle` Rust primitive: opens a file for writing (create/truncate) returning a `WriteHandle`; this splits the current `write(DirCap, path, String)` path to allow tinct-level pipe construction (`src/builtins_io.rs`, `src/type_env.rs`)
- [ ] Once `raw-create` lands, rewrite `copy` in tinct: `[fn [cap src dst] [close [write-handle [raw-create cap dst] [slurp [open cap src Readable Text]]]]]`; remove `copy` Rust builtin from `standard_builtins()` (`stdlib/io.llt`, `src/builtins_io.rs`)
- [ ] Change `cap-data` to return `Null` (empty dict `[]`) when the capability name is not present instead of erroring — this makes it a proper nullable lookup compatible with `get-or` and `has?` patterns (`src/builtins_io.rs`)
- [ ] Once `cap-data` returns null on miss, rewrite `has-cap?` in tinct as `[fn [h cap] [not [null? [cap-data h cap]]]]`; remove `has-cap?` Rust builtin (`stdlib/io.llt`, `src/builtins_io.rs`)
- [ ] Investigate and remove the vestigial Rust `http-get` builtin (the `HttpConn` form using `Value::HttpConn`/reqwest client directly): verify it is not called by any corpus tests, `stdlib/net.llt`, or user-facing code; `net.llt`'s `http-get` already implements HTTP without it (`src/builtins_io.rs`, `src/type_env.rs`)
- [x] Add `str-index-of : Str → Str → Int` Rust builtin: native O(n) substring search returning start byte-index or -1 on miss; wraps `str::find`; replaces the O(n²) `str-find-impl` in prelude with an O(n) call (`src/builtins_string.rs`, `src/type_env.rs`, `stdlib/prelude.llt`)
- [ ] Once `str-index-of` lands, rewrite `str-contains?`, `starts-with?` (string form), `ends-with?` (string form) in `stdlib/strings.llt` as tinct wrappers; remove the three Rust builtins from `standard_builtins()` (`stdlib/strings.llt`, `src/builtins_string.rs`)
- [ ] Add `str-map-chars : (Str → Str) → Str → Str` Rust builtin: map a tinct function over Unicode codepoints, returning a new string; unlocks `upper`, `lower`, and character-level transforms as tinct stdlib functions (`src/builtins_string.rs`, `src/type_env.rs`)
- [ ] Once `str-map-chars` lands, rewrite `upper` and `lower` in `stdlib/strings.llt` as tinct functions using `str-map-chars` + `char-code`/`chr` arithmetic for ASCII fast path; remove Rust builtins (`stdlib/strings.llt`, `src/builtins_string.rs`)
- [x] Add `trim-start : Str → Str` and `trim-end : Str → Str` Rust builtins: strip leading/trailing whitespace from one end only (`src/builtins_string.rs`, `src/type_env.rs`)
- [ ] Add `regex-match? : Str → Str → Bool` Rust builtin: test if a regex pattern matches anywhere in a string using the `regex` crate; unlocks the `pattern` constraint in `validate` and other regex-dependent stdlib functions as tinct code (`src/builtins_string.rs`, `src/type_env.rs`)
- [ ] Once `regex-match?` lands, rewrite `validate`'s `pattern` constraint check in tinct; identify which other parts of `validate` can move to tinct vs what must remain Rust (`stdlib/prelude.llt` or `src/builtins_meta.rs`)
- [ ] Make `builtin-sort` accept an optional comparator argument: `builtin-sort : ((a → a → Bool)? → Dict → Dict)`; when provided, use comparator instead of natural type ordering; this allows `sort` and `sort-by` in prelude to both reduce to one Rust primitive (`src/builtins_seq_prim.rs`, `src/type_env.rs`)
- [ ] Verify `stdlib/prelude.llt` imports exactly: `rust::core`, `rust::string`, `rust::collection`, `rust::json`, `rust::meta`; no bare Rust primitive references outside these groups; no `builtin-*` names remain (`stdlib/prelude.llt`)
- [ ] Verify `stdlib/io.llt`, `stdlib/net.llt`, `stdlib/math.llt`, `stdlib/datetime.llt`, `stdlib/encoding.llt` each open with exactly one `[include %rust "..."]` and build entirely on prelude exports + their imported group (`stdlib/*.llt`)
- [ ] Verify that intentionally unexported primitives (`eval-ast`, `gensym`, `llt-repr`, `tag-of`, `variant`, `decimal`, `big-int`, `proxy`) are not accessible from user code — write corpus tests confirming `undefined variable` (`tests/corpus/eval/`)
- [ ] Update `doc/11-stdlib.md §Rust-Native vs Tinct-Implemented Boundary` to document the `%rust` virtual module system and which modules each stdlib file imports (`doc/11-stdlib.md`)

---

## Internal Integrity

### primitive-privacy: Rust virtual modules + bootstrap env isolation

See `doc/whatif/builtin-privacy.md` (redesigned 2026-05-13). **Spec chapters:** `doc/11-stdlib.md §Rust-Native vs Tinct-Implemented Boundary`.

**Goal:** No Rust builtin is available to user code by default — not even `+`, `error`, or `=`. The bootstrap env contains only `include` and the injected caps. `%rust` virtual modules give stdlib files scoped access to Rust primitive groups. User code gets only what prelude exports.

**Already done:**
- [x] Migrate `stdlib/macros.llt`, `stdlib/path.llt`, `stdlib/toml-lite.llt` — no `builtin-*` calls remain
- [x] `create_root_env()` / `inject_prelude_aliases()` split in `src/builtins.rs`
- [x] T009 type-checker warning for `builtin-*` references outside `prelude.llt`
- [x] Remove vestigial `http-get` Rust builtin (HttpConn/reqwest form) — done 2026-05-13

**Phase 1 — `%rust` virtual module infrastructure:**
- [ ] Add `Value::RustRegistry` variant to `src/value.rs`: opaque Rust value (no payload; PartialEq trivially false, Display `"<rust-registry>"`); this is the type of `%rust` — user code cannot construct or name it (`src/value.rs`)
- [ ] Implement `rust_module(name: &str) -> Rc<RefCell<Environment>>` in `src/builtins.rs`: dispatches on module name (`"core"`, `"string"`, `"collection"`, `"io"`, `"net"`, `"math"`, `"datetime"`, `"bytes"`, `"json"`, `"meta"`) to return an env containing exactly the named primitive group; returns error for unknown names (`src/builtins.rs`)
- [ ] Extend the include resolver in `src/imports.rs`: when cap is `Value::RustRegistry`, call `rust_module(path)` instead of doing filesystem I/O; no DirCap check, no BLAKE3 hash, no cycle detection — virtual modules are pure in-memory lookups (`src/imports.rs`)
- [ ] Remove `builtin-*` aliases entirely from `src/builtins.rs` — `inject_prelude_aliases()` and all its registrations are no longer needed; prelude uses `%rust` modules instead (`src/builtins.rs`)

**Phase 2 — bootstrap env and env chain:**
- [ ] Replace `create_root_env()` with `create_bootstrap_env()` in `src/builtins.rs`: contains ONLY `include` (the special form binding) and `%rust` (a `Value::RustRegistry` sentinel); no other builtins (`src/builtins.rs`)
- [ ] Update `build_prelude_env` in `src/imports.rs`: evaluate `prelude.llt` in `create_bootstrap_env()` (prelude uses `[include %rust "core"]` etc. to access primitives); the prelude output env becomes the parent of the user env — user env does NOT inherit any primitive env directly; libdir-loaded stdlib files (io.llt, net.llt, etc.) are evaluated in the user env (which has prelude exports), and they use their own `[include %rust "..."]` at the top of each file to access their primitive group (`src/imports.rs`, `src/builtins.rs`)
- [ ] **[THE SWITCH]** Gate: after both phases above land, flip `build_prelude_env` to use `create_bootstrap_env()` instead of the current `create_root_env()`; run the full test suite — any primitive not imported via `[include %rust "..."]` in prelude or stdlib will surface as `undefined variable` (`src/imports.rs`)

**Phase 3 — rewrite stdlib files to use `[include %rust "..."]`:**
- [ ] Rewrite `stdlib/prelude.llt` to open with `[include %rust "core"]`, `[include %rust "string"]`, `[include %rust "collection"]`, `[include %rust "json"]`, `[include %rust "meta"]`; remove all bare references to Rust primitives not in these groups; remove all `builtin-*` references (`stdlib/prelude.llt`)
- [ ] Rewrite `stdlib/io.llt` to open with `[include %rust "io"]`; all other primitives it uses (str, error, etc.) come from prelude which is already in scope (`stdlib/io.llt`)
- [ ] Rewrite `stdlib/net.llt` to open with `[include %rust "net"]` (`stdlib/net.llt`)
- [ ] Rewrite `stdlib/math.llt` to open with `[include %rust "math"]` (`stdlib/math.llt`)
- [ ] Rewrite `stdlib/datetime.llt` to open with `[include %rust "datetime"]` (`stdlib/datetime.llt`)
- [ ] Rewrite `stdlib/encoding.llt` to open with `[include %rust "bytes"]` (`stdlib/encoding.llt`)
- [ ] Rewrite `stdlib/strings.llt` — verify it uses only prelude-exported names; no `%rust` needed if it builds purely on prelude (`stdlib/strings.llt`)

**Phase 4 — cleanup and tests:**
- [ ] Remove T009 type-checker warning (no longer needed — `builtin-*` names don't exist) (`src/typecheck.rs`)
- [ ] Tests: tinct file with no includes produces `undefined variable` for `+`, `error`, `map`; `prelude.llt` itself works; `[include %libdir "io.llt"]` works; user code cannot call `[include %rust "io"]` (undefined variable: `%rust`) (`tests/corpus/eval/`)

---

## Miscellaneous

### misc-gaps: Miscellaneous small gaps across the codebase

Accepted 2026-05-11. See `doc/whatif/multi-line-strings.md` (triple-quote lexer). Genuine deferred items from the `http-sessions` and `connector-tls` sprints. Two correctness/quality gaps in the evaluator noted in source comments. Extends `--cap-fs` and `--cap-file` with fine-grained read/write/list permissions.

- [ ] Add `TripleQuotedString(String)` and `TripleInterpolatedString(Vec<InterpolatedPart>)` token types to `src/lexer.rs`: detect `"""` at the start of a string context, consume content until the closing `"""`, emit accordingly; then in `src/parser.rs` desugar `TripleQuotedString(s)` → `[unindent s]` and `TripleInterpolatedString(parts)` → `[unindent i"..."]` directly in the parser (not as a stdlib macro, since macros cannot intercept token patterns) (`src/lexer.rs`, `src/parser.rs`)
- [ ] Tests: `quic-open-datagram` + `send-datagram` + `recv-datagram` round-trip corpus test; `http3-session` concurrent request (two sequential requests on one session succeed); QUIC datagram type error on wrong handle type (`tests/corpus/eval/`)
- [ ] **Unquote in nested positions** (`src/eval.rs:1343`): The `eval_quote` fallback arm (`_ =>`) calls `ast_to_dict_expr` which does not recognize `Expr::Unquote`/`Expr::UnquoteSplice` in nested positions; add a recursive `eval_quote_expr` pre-pass in `src/eval.rs` that walks the full `Expr` tree — when it encounters `Expr::Unquote(inner)`, evaluate `inner` and substitute the result as a serialized AST value node; when it encounters `Expr::UnquoteSplice(inner)` in a list position, splice the evaluated sequence; all other nodes recurse unchanged; replace the `_ =>` arm with a call to `eval_quote_expr` then `ast_to_dict_expr`; `ast_to_dict_expr` is unchanged (`src/eval.rs`); add corpus tests for nested `[unquote ...]` inside call args, dict values, and seq literals (`tests/corpus/eval/`)
- [ ] Extend `--cap-file` argument parsing in `src/main.rs`: same extended syntax — if mode starts with `[`, parse as `[Cap1 Cap2 ...]` list (valid names: `Readable`, `Writable`, `Appendable`, `Binary`); no `:mode` suffix → open file read-write (equivalent to `rw`); retain existing `r`/`rb`/`w`/`wb` letter shorthands for backward compat (`src/main.rs`)
- [ ] Implement `open` write and append paths in `builtin_open`: the `Writable` and `Appendable` flag branches currently return "not yet implemented" (`src/builtins_io.rs:197,371`); implement using `dir.open_with(path, OpenOptions::new().write(true).create(true).truncate(true))` for Writable and `.append(true)` for Appendable; wrap result in `Value::WriteHandle` with appropriate caps (`src/builtins_io.rs`)
- [ ] Register capability flag nominal unit types in `TypeEnv`: `Readable`, `Writable`, `Listable`, `Statable`, `Appendable`, `Deletable`, `Renameable` — each as a singleton `Type::NominalTag` (no payload); register `%pwd` and `--cap-fs` DirCaps using BAS intersection types; update builtin type signatures using `Type::Intersection([DirCap, Flag])`: `list-dir` → `Intersection([DirCap, Listable])`, `open "r"` → `Intersection([DirCap, Readable])`, `open "w"` → `Intersection([DirCap, Writable])`; the subtyping `Intersection([DirCap, Readable, Writable]) <: Intersection([DirCap, Writable])` holds automatically under BAS intersection elimination (`src/type_env.rs`, `src/types.rs`)
- [ ] Add `narrow` overload for DirCap: `[narrow cap@[[all DirCap Flag1 ...]] FlagName...]` produces a new DirCap with the intersection of source permissions and requested flags; the return type is `Intersection([DirCap, requested-flags])` — a BAS intersection narrower than the input; runtime error if a requested flag is not held in the source `DirPerms`; `[narrow cap Subtree "path"]` restricts the directory root to a subdirectory and returns the same intersection type with an updated root path (`src/builtins_io.rs` or new `src/builtins_cap.rs`)
- [ ] Tests: `--cap-fs root=.:r` → `list-dir` succeeds, `open "w"` fails; `--cap-fs data='./d:[Readable Statable]'` → read succeeds, `list-dir` fails; `--cap-file cfg=Cargo.toml` (no mode) → read-write handle; extended syntax `--cap-file cfg='Cargo.toml:[Readable]'` → read-only handle; `narrow` reduces permissions; `narrow` to non-held flag errors (`tests/corpus/eval/`, `tests/corpus/cli/`)

---

## Tooling

### tinct-hosted-formatter: Implement stdlib/formatter/format.llt

Accepted 2026-05-05. See `doc/whatif/completed/tinct-hosted-formatter.md` for the full design.
The Rust formatter (`src/formatter.rs`) is retained for LSP use; this formatter receives the AST dict from `ast_to_dict` and returns formatted source as a tinct string.

- [ ] Implement `stdlib/formatter/compact.llt` and `stdlib/formatter/pretty.llt` as tinct programs that receive `%` as the AST dict (from `ast_to_dict(Some(src), Some(comments))`) and return formatted source; wire `tinct fmt --compact`/`--pretty` to invoke these via the evaluator
- [ ] Implement `stdlib/formatter/format.llt` as the full formatter — layout algorithm, indentation, comment attachment, multi-line decisions per `doc/whatif/completed/tinct-hosted-formatter.md`; wire to `tinct fmt` (default mode)
- [ ] The Rust formatter (`src/formatter.rs`) is retained for LSP use — add a `FormatterMode` enum to dispatch between Rust and tinct-hosted based on invocation context; LSP always uses Rust formatter
- [ ] Tests: round-trip corpus tests (format → re-parse → compare AST); test compact/pretty/full modes; test comment preservation

