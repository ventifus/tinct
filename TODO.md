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

### builtin-type-audit: Fix Unknown→Any/Never in builtin type registrations

Audit and fix incorrect `Type::Unknown` uses in `TypeEnv::with_builtins()` (`src/type_env.rs`).
`Unknown` = gradual-typing opt-out (consistency, not subtyping); `Any` = accepts anything within the lattice; `Never` = does not return.

- [x] `length`: remove stale `TODO(length-narrow-type)` comment and stale `RowTail::RowVar` reference; update registration to `Union(Dict, String, Bytes)` → `Int` since `length-narrow-type` sprint is already complete (`src/type_env.rs`)
- [x] `if` return: `Unknown` → `Any` for both branch params and return type (`src/type_env.rs`)
- [x] `append` value param: second param `Unknown` → `Any` — it accepts any value but is not a type-checking opt-out (`src/type_env.rs`)
- [x] `apply` return: `Unknown` → `Any` (`src/type_env.rs`)
- [x] `try` return: `Unknown` → `Any` (`src/type_env.rs`)
- [x] `force`: `(Unknown) → Unknown` — change to pass-through TypeVar or `(Any) → Any` (`src/type_env.rs`)
- [x] `error` return: `Unknown` → `Never` — `error` always throws, never returns a value (`src/type_env.rs`)
- [x] `slurp` return: `Unknown` → `String` — reads file contents as a string (`src/type_env.rs`) *(actual: Union(Str,Bytes))*
- [x] `env` return: `Unknown` → `String` — reads environment variable as a string (`src/type_env.rs`) *(actual: Union(Str,Null))*
- [x] Add param names to `with_builtins()` registrations for common builtins (aids LSP hover): `set`, `get`, `has?`, `append`, `merge`, `if`, `map`, `filter`, `reduce` at minimum
- [x] **Prelude follow-ups (batch B)** — gate on `constraint-annotations` sprint landing first (fixes `fn@[...]` positional-union path); note: these same functions are verified in `hkt-mappable-appendable` after Mappable lands — apply the edits here, verification happens there: `when`/`unless` → `fn@[a Null] [pred body@a]`, `cond` → `fn@[a Null] [branches]`, `and` → `fn@[a Bool] [p b@a]`, `or` → `fn@a [a@a b@a]`, `get-or` → `fn@a [xs key default@a]`, `find-first` → `fn@[a Null] [pred xs@Seq@a]`, `find-first-or` → `fn@a [pred xs@Seq@a default@a]`; note: verify `when`/`unless` → the `[]` return is typed `Record` (empty dict) not `Null` — the annotation `fn@[a Null]` assumes the empty-dict return is `Null`; if `[]` is typed as `Record` rather than `Null`, adjust the annotation accordingly (`stdlib/prelude.llt`)
- [x] Fix `result` monad dict description in `doc/11-stdlib.md` line ~554: currently lists `map:`, `or:`, `ok?:` fields that don't exist; actual prelude has only `bind: and-then  pure: result-ok` (`doc/11-stdlib.md`)
- [x] Fix `assert` short-form table entry in `doc/11-stdlib.md` line ~582: still shows `[fn [cond msg] ...]`, should show `fn@Unknown [cond msg@String]` to match the Prelude Type Signatures table (`doc/11-stdlib.md`)
- [x] [Major] `doc/11-stdlib.md:302` stale Rust builtin count: doc shows "189 Rust-native builtins" — verified 189 is correct (`doc/11-stdlib.md`)
- [x] [Major] `stdlib/prelude.llt:31-46` phantom aliases: the comment block lists 28 stable `builtin-*` aliases but `create_root_env()` only registers 12; remove the 16 phantom entries (`builtin-seq`, `builtin-head`, `builtin-tail`, `builtin-collect`, `builtin-range`, `builtin-repeat`, `builtin-cycle`, `builtin-iterate`, `builtin-unfold`, `builtin-join`, `builtin-concat`, `builtin-first`, `builtin-last`, `builtin-rest`, `builtin-cons`, `builtin-reverse`, `builtin-sort`, `builtin-get`) from the comment (`stdlib/prelude.llt`)
- [x] [Minor] `stdlib/prelude.llt:440` `trunc` uses `gte-impl` in the public dict instead of `>=`: change `[builtin-if [gte-impl x 0] ...]` to `[builtin-if [>= x 0] ...]` — `>=` is defined at line 399 and available in the public dict scope (`stdlib/prelude.llt`)
- [x] [Major] `src/lib.rs:237` depth-exceeded during display serialization emits E099 (Internal) instead of E040 (DepthExceeded): change `EvalError::internal("depth exceeded...")` to `EvalError::depth_exceeded(...)` so depth errors in `value_to_display_string` have the correct error code and category (`src/lib.rs`)
- [x] [Minor] `Substitution::apply()` allocates a `HashSet` for compound concrete types even when there are no inference variables; add an early `has_inference_vars()` guard so concrete types short-circuit without allocation (`src/type_unify.rs`)

### infer-fn-typevar: Fix unannotated param TypeVar inference and gated prelude follow-ups

These two items were gated out of `builtin-type-audit` because the `infer_fn` TypeVar fix is a significant behavior change that requires its own audit sprint; batch A prelude annotations depend on it landing first.

- [ ] `infer_fn` unannotated params: change `None => Ok(Type::Unknown)` (line 3074 `src/typecheck.rs`) to `None => Ok(state.new_type_var(span))` — unannotated params should get fresh TypeVars for proper HM inference, not Unknown (gradual opt-out). This enables constraint propagation (e.g. `[fn [a b] [= a b]]` infers `Equatable a => Fn@Bool [a a]`) and LSP hover shows `a` not `Unknown`. This is a significant behavior change — audit for test breakage.
- [ ] **Prelude follow-ups (batch A)** — gate on BOTH `error → Never` AND `infer_fn` TypeVar fix above landing first:
  - `fold` (prelude.llt:725): change `fn@Unknown` → `fn@a [f@Fn init@a xs]` — `a` in `fn@a` and `init@a` binds return type to the accumulator type (`stdlib/prelude.llt`)
  - `assert` (prelude.llt:1095): change `fn@Unknown` → `fn@Bool` — once `error` is typed `Never`, inference produces `Bool | Never = Bool`, making `@Bool` correct (`stdlib/prelude.llt`)

### scc-inference: SCC-based binding group analysis for letrec polymorphism

Research done — see `doc/whatif/inference-completeness.md`. Implements Tarjan SCC decomposition within DICT-GEN to enable independent generalization of non-mutually-recursive bindings (fixes letrec monomorphism and nested dict let-polymorphism). See doc/whatif/inference-completeness.md §SCC Binding Group Analysis.

- [ ] Add Tarjan SCC computation over the dependency graph of a letrec dict's entries: for each entry, collect the set of other entries it references (by name); run Tarjan to produce topologically-sorted SCCs (`src/typecheck.rs`)
- [ ] Extend DICT-GEN Pass 4 generalization: instead of generalizing all entries together at the end, generalize each SCC independently in topological order — entries in a single-node SCC (no recursive reference to itself) are generalized immediately; entries in a multi-node SCC are generalized together after the whole SCC is typed (`src/typecheck.rs`)
- [ ] Reject polymorphic recursion explicitly: when a recursive call's inferred type is `App(T, a)` and `T` is not the same variable as the enclosing binding's TypeVar, emit `TypeError` "polymorphic recursion is not supported — add a type annotation" (`src/typecheck.rs`)
- [ ] Extend nested dict let-polymorphism: inner dict entries (not just top-level) that pass SCC analysis are eligible for DICT-GEN generalization at their respective levels (Kiselyov 2013 levels model); inner entries currently stay at the outer level and remain monomorphic (`src/typecheck.rs`)
- [ ] Tests: `[let [id: [fn [x] x]] [id 1] [id "a"]]` — two uses of `id` at different types succeed; simple mutual recursion (`even?`/`odd?`) types correctly; non-recursive inner binding generalizes; polymorphic recursion rejected with clear error (`tests/corpus/eval/typecheck/`)

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

- [ ] Add post-processing function `scan_type_quality(type_map, ast, diagnostics: &mut Vec<TypeDiagnostic>)` in `src/typecheck.rs`: called after `typecheck_file` completes, receives the type map and original AST; emits diagnostics at base level (Info/Warn), `--strict` bump applied at CLI/LSP layer (`src/typecheck.rs`)
- [ ] Unknown detection: for each binding's `TypeScheme`, walk all type positions (return type, param types, dict entry types, intermediate types); check if `Unknown` appears; for each occurrence, inspect the original AST annotation — if `@Unknown` was explicitly written, mark as "explicit"; otherwise mark as "inferred" (`src/typecheck.rs`)
- [ ] TypeAssert detection: for `[@Unknown expr]` TypeAssert nodes in the AST, treat the same as an explicit `@Unknown` annotation — silent (non-strict) / warn (strict) (`src/typecheck.rs`)
- [ ] Emit Unknown diagnostics: inferred Unknown → `TypeDiagnostic { level: Warn }` (bumped to Err by `--strict`); explicit Unknown → `TypeDiagnostic { level: Info }` (bumped to Warn by `--strict`) (`src/typecheck.rs`)
- [ ] Over-broad annotation detection: for each binding with a declared return type annotation and an inferred type, check `is_subtype(inferred, declared) && !is_subtype(declared, inferred)`; when true, emit `TypeDiagnostic { level: Info }` suggesting the inferred type as the tighter annotation (bumped to Warn by `--strict`) (`src/typecheck.rs`, `src/type_unify.rs`)
- [ ] Over-broad detection covers: `fn@Number` when body infers `Int`; `param@Dict` when inference constrains to a specific record; `@Top` / `@Any` when a precise type is inferred; union annotations `@[Int String]` when inference produces only one branch (`src/typecheck.rs`)
- [ ] Wire `scan_type_quality` into `typecheck_file`; pass the `Vec<TypeDiagnostic>` through the return; `--strict` bump is applied at emission time in the CLI/LSP layer, not in the scanner itself — the scanner always emits at the base level (`src/typecheck.rs`, `src/main.rs`)
- [ ] Tests: corpus tests with `=== warn` sections for: inferred Unknown warns; explicit `@Unknown` silent in default; explicit `@Unknown` warns in `--strict`; inferred Unknown errors in `--strict`; `[@Unknown expr]` same as explicit; `fn@Number` with `Int` body warns "consider @Int"; `param@Dict` with specific record warns; `--strict` escalation (`tests/corpus/eval/typecheck/`)

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

- [ ] Add `@Label` simple annotation form to `resolve_type_name` in `src/typecheck_annot.rs`: when annotation is `Simple("Label")`, create a fresh anonymous Label-kinded TypeVar (system-generated name), register `kind_env[fresh] = Kind::Label`; parallel to `@Operator` which creates an anonymous Operator-kinded TypeVar (`src/typecheck_annot.rs`)
- [ ] Add `[label: name]` property dict form to the annotation resolver: when a `PropertyDict` annotation has exactly one entry with key `label` and a bare-name value, create a named Label-kinded TypeVar, register it in `kind_env` and `ann_mapping`; use when the label TypeVar must be referenced elsewhere in the type scheme (`src/typecheck_annot.rs`)
- [ ] Remove the `key@"l"` string-literal mechanism for Label TypeVars from `src/typecheck_annot.rs` — it was introduced in `hkt-field-access` and has no users outside that sprint; restore whatever pre-hkt-field-access behavior existed for string literals in annotation position (i.e. remove the code that was added, do nothing special) (`src/typecheck_annot.rs`)
- [ ] Update `stdlib/prelude.llt` `get`/`get-or` annotations: use the anonymous form since the label TypeVar is never referenced by name; remove `constraint: [HasField l d a]` entirely; correct annotations: `get: [fn@[return: a] [key@Label  dict@d] ...]` and `get-or: [fn@[return: a] [key@Label  dict@d  default@a] ...]` (`stdlib/prelude.llt`)
- [ ] Update `src/type_env.rs` scheme registration for `get`/`get-or` to match the anonymous label form; the Rust-side scheme stores the HasField constraint as a generated constraint, not user-written
- [ ] Update `doc/whatif/completed/hkt-monads.md §Field Access Typing` and `doc/06-type-inference.md §HasField`: document both `@Label` (anonymous) and `@[label: l]` (named) forms with examples; replace `key@"l"` throughout; clarify HasField is never user-written
- [ ] Remove the stale note at the bottom of `hkt-field-access` sprint about `constraint-annotations` dependency for HasField syntax — both the dependency and the HasField annotation syntax were incorrect
- [ ] Tests: `key@Label` generates HasField constraint and returns precise field type; `key@[label: l]` where same `l` is used in two parameters works; `get`/`get-or` return precise types at call sites with string literal keys (`tests/corpus/eval/typecheck/`, `tests/lsp_corpus_tests.rs`)

### hkt-kind-inference: Kind checking pass and Operator-kinded class resolution

See `doc/whatif/completed/hkt-monads.md` §Kind Checking, §Typeclass Resolution for HKT. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §Formal Type Rules`.

- [ ] Add kind inference pre-pass in `src/typecheck.rs`: walk class method signatures, look up parameter kinds from `kind_env`; assign `Kind::Operator` to parameters annotated `@Operator` or constrained by an Operator-kinded class
- [ ] Implement `KIND-OPERATOR` validation: `App(f, a)` during annotation resolution — `f : Operator`, `a : *` → valid; `f : *` → `TypeError` "kind mismatch: expected `* → *`, got concrete type" (`src/typecheck.rs`, `src/typecheck_annot.rs`)
- [ ] Enforce rank-1 restriction: reject `App(Operator("f"), Operator("g"))` (both Operator-kinded) — emit `TypeError` "rank-2 type constructor application is not supported"; note that multiple flat Operator vars in a single method type (like `traverse`'s `f` and `t`) are correctly rank-1 and NOT rejected (`src/typecheck.rs`)
- [ ] Extend `ClassEnv` lookup for Operator-kinded class params: unify instance head against `App(m, _)` using UNIFY-APP (`src/type_env.rs`); the `resolve_instance` freshening fix (freshen free type vars via `instantiate_at_level`, capture not discard `temp_subst`) is **implemented in `hkt-mappable-appendable`** where it is first needed for `AppendableSeq [Seq b]`; this sprint's task is to wire up the Operator-kinded lookup path, not to implement the freshening
- [ ] Add `App` type inference: when binding infers `App(Operator("m"), a)`, apply UNIFY-OPERATOR against known instance heads; update `InferState.subst` (`src/typecheck.rs`)
- [ ] Normalize at instance resolution: `App(Seq_ctor, T) → Type::Seq(T)`; `App(App(Map_ctor, K), V) → Type::Map(K, V)`; `App(Result_ctor, T)` stays as `App` (`src/typecheck_annot.rs`)
- [x] Assign error code `E091` for kind mismatch errors in `src/error.rs`; add to `doc/10-errors.md` all three tables (variant catalog, codes table, categories table) — `hkt-doc-lsp` will verify these entries exist, not re-add them
- [ ] Tests: kind mismatch errors with `[E091]` prefix, `App(Result, Int)` inferred from `[Ok 42]`, rank-1 violation rejected (but multiple flat Operator vars in one method type like `traverse` are NOT rejected), Operator-kinded class constraint resolution (`tests/corpus/eval/typecheck/`)

### hkt-do-macro: Implement [do] macro — explicit form first, inferred form second

See `doc/whatif/completed/hkt-monads.md` §`[do]` Inference. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §[do] Inference`.

Note: The explicit `[do monad steps...]` desugaring needs the ClassEnv for monad dict dispatch setup but not the full kind inference pass — explicit form can proceed independently; inferred form requires `hkt-kind-inference`.

- [ ] Implement explicit `[do monad steps...]` desugaring in `stdlib/macros.llt` (or `stdlib/prelude.llt` if `stdlib-defmacro` has already landed and merged macros.llt away): classify each step as binding (`[x: expr]`) or non-binding by inspecting the AST dict shape; bindings → `[monad.bind expr [fn [x] ...]]`; non-bindings → `[monad.bind expr [fn [_] ...]]`; `[do monad]` with no steps → `[monad.pure []]`; `[do]` with zero args → error
- [ ] Add `expected_return: Option<Type>` field to `InferState` (`src/types.rs`) — set by `infer_fn` before descending into the function body when the function has an explicit return type annotation; used by the `[do]` inferred form resolution; using `InferState` (not a parameter) avoids a cascading `infer_expr` signature change
- [ ] Implement inferred `[do steps...]` form: emit `[do %do-infer steps...]` sentinel (`Expr::VarRef("%do-infer")`) at macro-expand time; in `src/typecheck.rs` `infer_expr`, when the `[do]` form has `%do-infer` as its monad, resolve sentinel via: (1) `state.expected_return` unifying with `App(m, _)` for a registered Monad; (2) first binding RHS type `App(m, a)` for a known Monad; (3) if unresolved, emit error; the runtime always sees `[monad.bind ...]` with a concrete dict (inferred form substitutes the resolved monad name before eval)
- [ ] Emit "cannot infer monad for `[do]` — add an explicit monad argument or annotate the enclosing function's return type"
- [ ] Tests: `[do result ...]` three-step success, `[Err "fail"]` propagation (short-circuit), explicit `[do]` with any `bind:`-carrying dict (backward compat), inferred `[do]` from `@Result` annotation, inferred from first binding type, missing-monad error, `[do monad]` with no steps → `[monad.pure []]` (`tests/corpus/eval/`)

**Depends on:** `hkt-kind-inference` (inferred form only)

### hkt-mappable-appendable: Rewrite Mappable and Appendable from hardcoded to class-based

See `doc/whatif/completed/hkt-monads.md` §The Typeclass Hierarchy §Mappable, §Appendable. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §The Typeclass Hierarchy`.

- [ ] Implement `resolve_instance` freshening fix in `src/type_env.rs`: freshen all free type vars in `inst.instance_type` via `instantiate_at_level` before unification (current code does NOT do this); capture `temp_subst` bindings after successful unification (currently discarded after `is_ok()` check); apply `temp_subst` bindings to the instance's method implementations so `b = T` threads through `append`/`empty` in `AppendableSeq [Seq b]`; this fix is general — it enables parameterized instance heads of any kind, not just Operator-kinded (note: `hkt-kind-inference` wires up the Operator-kinded lookup path; this sprint implements the freshening that makes it work)
- [ ] Write `Mappable` class + `MappableSeq`/`MappableRecord` instances in `stdlib/prelude.llt`; update `ClassDecl` kind annotation for Mappable param to `Kind::Operator` in `InferState::new()` (`src/types.rs`)
- [ ] Write `Appendable` class (kind-`*`) + `AppendableStr`/`AppendableSeq [Seq b]`/`AppendableRecord` instances in `stdlib/prelude.llt`; `AppendableSeq` parameterized head relies on resolve_instance freshening fix
- [ ] Remove `Mappable` from `satisfies_constraint` hardcoded match + placeholder ClassDecl — only after verifying resolve_instance handles Operator-kinded Mappable end-to-end
- [ ] Remove `Appendable` from `satisfies_constraint` — same gate condition
- [ ] Update `$map`/`$filter` type sigs in `src/type_env.rs` to use `Mappable f`; update `$concat`/`$conj` to use `Appendable a`
- [ ] Write `Equatable` class + instances for `Int`, `Str`, `Bool`, `Float`; remove from `satisfies_constraint` (`stdlib/prelude.llt`, `src/type_unify.rs`)
- [ ] Write `Comparable` class (extends Equatable) + instances for `Int`, `Str`, `Float`; remove from `satisfies_constraint` (`stdlib/prelude.llt`)
- [ ] Write `Showable` class + instances for `Int`, `Str`, `Bool`, `Float`, `Null`; remove from `satisfies_constraint`; `Numeric` stays hardcoded (`stdlib/prelude.llt`)
- [ ] Verify and confirm the prelude union-annotation follow-ups (tracked in `builtin-type-audit` sprint batch B) still type-check correctly now that Mappable is a real class: `when`/`unless` → `fn@[a Null]` (note: `[]` empty-dict return is typed as `Record`, not `Null` — verify correct annotation choice), `cond`, `and`/`or`, `get-or`, `find-first`/`find-first-or`; annotate `zip` once Mappable is confirmed working for both Seq×Seq and Dict×Dict cases
- [ ] Tests: Mappable on user type (success), `map` on non-Mappable `Int` (error), `AppendableSeq [Seq b]` for different element types, `AppendableStr` string concat, Equatable/Comparable/Showable constraints on user types (`tests/corpus/eval/typecheck/`)

**Depends on:** `hkt-kind-inference`

### hkt-stdlib: Functor/Applicative/Monad/Foldable/Traversable hierarchy, Maybe, generic functions

See `doc/whatif/completed/hkt-monads.md` §The Typeclass Hierarchy, §Generic Functions. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §The Typeclass Hierarchy`, `§Generic Functions`.

- [ ] Write `Functor` class + `FunctorResult`/`FunctorSeq` instances (`stdlib/prelude.llt`)
- [ ] Write `Applicative` class (extends Functor, `pure` + `lift2`) + `ApplicativeResult`/`ApplicativeSeq` instances
- [ ] Write `Monad` class (extends Applicative, `bind`) + `MonadResult`/`MonadSeq` instances
- [ ] Write `Foldable` class (`fold`, `to-seq`) + `FoldableSeq`/`FoldableRecord`/`FoldableResult` instances; `FoldableSeq.fold = reduce`; **`FoldableResult.to-seq: [fn [r] [match r [Ok a] [a] [Err _] []]]`** — wraps the single `Ok` value in a singleton Seq `[a]`, NOT returning the bare value `a` (Result holds one element, not a collection)
- [ ] Add `Maybe` ADT (`[type [a] [Some a] | [None]]`) + `FunctorMaybe`/`ApplicativeMaybe`/`MonadMaybe`/`TraversableMaybe` instances; re-export `Some`/`None` following `Ok`/`Err` pattern
- [ ] Write `Traversable` class (extends Functor + Foldable) + `TraversableSeq`/`TraversableResult`/`TraversableMaybe` instances; **`TraversableSeq.traverse` MUST use the primitive fold-based implementation** — NOT via generic `sequence`/`traverse` (which is circular and non-terminating): `[reduce [fn [acc x] [f.lift2 [fn [as a] [concat as [a]]] acc [f x]]] [f.pure []] xs]`
- [ ] Write generic `sequence` (Traversable-generic) and `traverse` (Traversable-generic) in `stdlib/prelude.llt`; write `forM`, `when`, `liftM2`
- [ ] Verify `sequence` short-circuits on first `Err`/`None` via Traversable instances; verify no evaluation of subsequent elements after failure
- [ ] Verify superclass method inheritance: each instance dict must carry all ancestor methods — `MonadResult.lift2` must be accessible (from `ApplicativeResult`), `MonadResult.fmap` must be accessible (from `FunctorResult`); add corpus tests for `MonadResult.lift2` and `MonadResult.fmap` dispatch; verify `ApplicativeSeq.pure = [fn [x] [x]]` wraps `x` in a one-element Seq, not returns bare value
- [ ] Tests: `sequence result [[Ok 1] [Err "fail"] [Ok 3]]` → `[Err "fail"]` (short-circuit), traverse over TraversableResult/TraversableMaybe, forM, `when false` (action not evaluated), liftM2, `[do MonadMaybe]` with None short-circuit, FoldableSeq.fold equals reduce, FoldableResult fold on Ok/Err, FoldableResult.to-seq `[Ok 42]` → `[42]` (singleton), `FoldableResult.to-seq [Err "x"]` → `[]` (empty) (`tests/corpus/eval/`)

**Depends on:** `hkt-do-macro`, `hkt-mappable-appendable`

### hkt-doc-lsp: doc/06 Type Classes section, LSP hover, error quality

See `doc/whatif/completed/hkt-monads.md §What Would Change`. **Spec chapters:** `doc/06-type-inference.md`, `doc/whatif/completed/hkt-monads.md`.

- [x] Move `doc/whatif/hkt-monads.md` to `doc/whatif/completed/hkt-monads.md` — already done
- [x] Update `doc/whatif/index.md` Accepted section with acceptance date 2026-05-11 — already done
- [ ] Write §Type Classes formal rules section in `doc/06-type-inference.md`: constraint generation, entailment, dictionary elaboration, instance resolution, superclass extraction, `UNIFY-OPERATOR`/`UNIFY-APP`/`KIND-OPERATOR`/`KIND-CLASS-PARAM` rules, parameterized instance head resolution
- [ ] Verify LSP hover shows `[Result Int]` for `App(Result, Int)` via Display (stub arm in `hkt-foundation-a`); **improve** `Expr::TypeApp` arm in `hover_at_expr` (`src/lsp/analysis.rs`) to display the resolved `App` type from the type map (same pattern as `Expr::Annotated` hover handling — the stub may only return a raw string)
- [ ] Kind error message quality: include annotation span, mismatched kinds, and hint — "kind mismatch at `f`: `Int` has kind `*`, expected `* → *` — annotate as `f@Operator`"
- [ ] Verify `E091` entries exist in `doc/10-errors.md` all three tables (should have been added in `hkt-kind-inference`); add missing entries only if that sprint omitted them
- [ ] Apply stdlib prelude annotation migrations: `min`/`max`/`sorted`/`sort-by` → `fn@[return: a constraint: [a: Comparable]] [xs@Seq@a] ...` (include param annotations); `fold`/`reduce` → add `doc:` strings (`stdlib/prelude.llt`)
- [ ] Tests: LSP hover for `Type::App` display, kind mismatch errors with `[E091]` prefix (`tests/lsp_corpus_tests.rs`, `tests/corpus/eval/errors/`)

**Depends on:** `hkt-stdlib`

---

## Macro Architecture

### stdlib-defmacro: Proper stdlib macro registration via ExpandResult

The macros.md design intends `[defmacro ...]` in stdlib to be available to user code — the same as user-defined macros. The current implementation is a workaround: stdlib macros are defined as `*-transformer` functions in `macros.llt` and pre-registered via a hardcoded `STDLIB_MACROS` table in `src/expand.rs`. This breaks for two reasons: (1) the table requires a Rust change for every new stdlib macro, and (2) transformer bodies in macros.llt can't use prelude functions because inner `expand_macros` runs at depth >0 with `create_root_env` (no prelude). The fix: make `ExpandResult` carry discovered macro registrations from stdlib expansion, and evaluate transformer bodies against the full stdlib env at depth 0.

- [ ] Add `pub discovered_macros: Vec<(String, Rc<Thunk>)>` to `ExpandResult` in `src/expand.rs`; when the `expand_macros` pass processes an `Expr::DefMacro` node, push the `(macro_name, transformer_thunk)` to `ExpandResult.discovered_macros` in addition to registering in the local `MacroEnv` (`src/expand.rs`)
- [ ] Change stdlib loading in `expand_macros`: after `create_stdlib_env()` returns, re-parse and partially-expand `stdlib/macros.llt` at depth 0 (with the full stdlib env available) to collect its `DefMacro` declarations; evaluate each transformer body against the full stdlib env and register in the outer `env_macro` — this replaces `register_stdlib_macros` entirely (`src/expand.rs`, `src/imports.rs`)
- [ ] Remove `register_stdlib_macros` and the `STDLIB_MACROS` hardcoded table from `src/expand.rs`; remove the `*-transformer` naming convention (no longer needed)
- [ ] Rewrite `stdlib/macros.llt` to use `[defmacro ...]` declarations directly, with transformer bodies written as normal tinct using prelude functions (`stdlib/macros.llt`): `[defmacro tmpl ...]` / `[defmacro do ...]` replace the `tmpl-transformer` / `do-transformer` function pattern; add `[defmacro begin [args] ...]` whose body uses `reduce`, `range`, `append` to collect args and emits `{type: "sequential"  exprs: arglist}`; once this sprint lands `[begin e1 e2 ...]` is available to all user code with no Rust registration
- [ ] Fold `stdlib/macros.llt` into `stdlib/prelude.llt`: move the `[defmacro ...]` declarations and their helper functions into prelude.llt (private helpers with `tmpl-` prefix already fit the existing naming convention); rewrite all `builtin-*` calls in the helpers to use normal prelude wrappers (`if`, `=`, `+`, etc.) — these were `builtin-*` only because macros.llt ran at depth>0 with no prelude; remove `load_stdlib_module("macros.llt")` from `src/builtins.rs` (`stdlib/prelude.llt`, `src/builtins.rs`)
- [ ] Tests: `[begin [a: 1] [+ a 2]]` → 3 in user code; `[begin [a: 1] [b: [+ a 1]] [+ a b]]` → 3; `[begin]` (no args) → empty dict; `i"Hello $name"` still works; `[do]` stub still fires; new stdlib macro added to prelude.llt without any Rust change (`tests/corpus/eval/`)
- [ ] Either use or delete the three Phase 2 automatic hygiene functions `rename_macro_bindings`, `collect_and_rename_bindings`, `rename_refs` in `src/expand.rs:952,966,1108` (all `#[allow(dead_code)]`, labeled "Phase 2 future") — if automatic hygiene is not in scope for this sprint, delete them and record the decision; if it is, integrate them (`src/expand.rs`)

---

## Syntax

### multi-line-strings: `unindent` stdlib function and `"""` macro

Accepted 2026-05-11. See `doc/whatif/multi-line-strings.md`. **Spec chapters:** `doc/02-syntax.md §2.3.6 Multi-Line Strings`, `doc/11-stdlib.md §Strings`. No lexer changes needed — literal newlines in `"..."` already work. `"""` is a parse-stage macro wrapping `[unindent "..."]`.

- [x] Add `unindent` to `stdlib/prelude.llt`: use sequential fn body — binding dict `[ls: [lines s]  n: [length [last ls]]  inner: [slice 1 -1 ls]]` followed by `[join "\n" [map [fn [l] [slice n [length l] l]] inner]]`; the binding dict's entries are in scope for the final expression via `Expr::Sequential` (`stdlib/prelude.llt`)
- [ ] Register `"""` and `i"""` as parse-stage macros in `stdlib/macros.llt`: `"""content"""` → `[unindent "content"]`, `i"""content"""` → `[unindent i"content"]`; the lexer already tokenizes the content correctly (`stdlib/macros.llt`)
- [x] Add note to `doc/02-syntax.md §String Literals` that `"..."` permits embedded literal newlines; document `"""..."""` and `i"""..."""` as the idiomatic indentation-stripping form; document `unindent` as the underlying function (`doc/02-syntax.md`)
- [x] Tests: `unindent` directly on a raw indented string, `"""..."""` value matches `[unindent "..."]`, `i"""..."""` with `$var` interpolation, single `"` inside triple-quoted content, empty lines preserved, `[trim [unindent ...]]` trailing-newline suppression (`tests/corpus/eval/`)

### multi-body-positions: Extend sequential multi-body to match arms and macro bodies

`Expr::Sequential` (multi-body let-binding) already works in `[fn ...]` bodies with no evaluator or type-checker changes needed. Extend the same rule to other body positions: wherever the parser has a natural delimiter after which it reads expressions until `]`, allow multiple expressions and wrap them in `Expr::Sequential`. No new keywords. **Spec chapters:** `doc/02-syntax.md §2.3.2 Special Forms`, `doc/04-functions.md`.

- [ ] Extend `[match ...]` arm parsing in `src/parser.rs`: after each arm's pattern, read expressions greedily until the next pattern-looking entry (a bracket starting with a pattern) or the closing `]`; if more than one expression, wrap in `Expr::Sequential`; the existing sequential semantics (intermediate dicts extend scope, last expr is result) apply unchanged (`src/parser.rs`)
- [ ] Extend `[defmacro ...]` body parsing in `src/parser.rs`: after the param list `[...]`, read remaining expressions as a body sequence; if more than one, wrap in `Expr::Sequential`; same treatment as `[fn ...]` bodies today (`src/parser.rs`)
- [ ] Update `src/formatter.rs`: when a match arm body is `Expr::Sequential`, format its expressions indented on separate lines (same as fn multi-body formatting) (`src/formatter.rs`)
- [ ] Update `doc/02-syntax.md` and `doc/04-functions.md`: document that `[match ...]` arm bodies and `[defmacro ...]` bodies accept multiple sequential expressions; clarify that `[if ...]` branches and call arguments do not (no body delimiter) (`doc/02-syntax.md`, `doc/04-functions.md`)
- [ ] Tests: match arm with binding dict + result expression, nested match arm multi-body, defmacro with multi-body, formatter round-trip of multi-body match arm (`tests/corpus/eval/`, `tests/corpus/format/`)

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

- [ ] Refactor `--cap-fs` argument parsing in `src/main.rs`: split on last `:` via `rsplit_once`; if no `:` present, grant full `DirPerms::full()`; if mode starts with `[`, parse as extended capability list; otherwise parse letter-by-letter accumulating bundles (`r`→Readable+Listable+Statable, `w`→Writable+Appendable+Deletable+Renameable, `a`→Appendable, `s`→Statable, `l`→Listable+Statable); unknown letter = startup error (`src/main.rs`)
- [ ] Extend `--cap-file` argument parsing in `src/main.rs`: same extended syntax — if mode starts with `[`, parse as `[Cap1 Cap2 ...]` list (valid names: `Readable`, `Writable`, `Appendable`, `Binary`); no `:mode` suffix → open file read-write (equivalent to `rw`); retain existing `r`/`rb`/`w`/`wb` letter shorthands for backward compat (`src/main.rs`)
- [ ] Add `DirPerms { readable, statable, listable, writable, appendable, deletable, renameable: bool }` struct to `src/value.rs`; add `perms: DirPerms` field to `Value::DirCap` and `Value::RevocableDirCap`; update all construction sites to use `DirPerms::full()` (`src/value.rs`)
- [ ] Implement `open` write and append paths in `builtin_open`: the `Writable` and `Appendable` flag branches currently return "not yet implemented" (`src/builtins_io.rs:197,371`); implement using `dir.open_with(path, OpenOptions::new().write(true).create(true).truncate(true))` for Writable and `.append(true)` for Appendable; wrap result in `Value::WriteHandle` with appropriate caps (`src/builtins_io.rs`)
- [ ] Enforce permissions in `builtin_open`: `readable` for `"r"`, `writable` for `"w"`, `appendable` for `"a"`; capability error `"DirCap: open requires <Readable|Writable|Appendable> permission"` on violation (`src/builtins_io.rs`)
- [ ] Enforce `listable` in `builtin_list_dir`; enforce `writable` in `builtin_write`/`builtin_write_atomic`; stubs for future `builtin_delete_file` (needs `deletable`) and `builtin_rename_file` (needs `renameable`) (`src/builtins_io.rs`)
- [ ] Register capability flag nominal unit types in `TypeEnv`: `Readable`, `Writable`, `Listable`, `Statable`, `Appendable`, `Deletable`, `Renameable` — each as a singleton `Type::NominalTag` (no payload); register `%pwd` and `--cap-fs` DirCaps using BAS intersection types; update builtin type signatures using `Type::Intersection([DirCap, Flag])`: `list-dir` → `Intersection([DirCap, Listable])`, `open "r"` → `Intersection([DirCap, Readable])`, `open "w"` → `Intersection([DirCap, Writable])`; the subtyping `Intersection([DirCap, Readable, Writable]) <: Intersection([DirCap, Writable])` holds automatically under BAS intersection elimination (`src/type_env.rs`, `src/types.rs`)
- [ ] Add `narrow` overload for DirCap: `[narrow cap@[[all DirCap Flag1 ...]] FlagName...]` produces a new DirCap with the intersection of source permissions and requested flags; the return type is `Intersection([DirCap, requested-flags])` — a BAS intersection narrower than the input; runtime error if a requested flag is not held in the source `DirPerms`; `[narrow cap Subtree "path"]` restricts the directory root to a subdirectory and returns the same intersection type with an updated root path (`src/builtins_io.rs` or new `src/builtins_cap.rs`)
- [ ] Tests: `--cap-fs root=.:r` → `list-dir` succeeds, `open "w"` fails; `--cap-fs data='./d:[Readable Statable]'` → read succeeds, `list-dir` fails; `--cap-file cfg=Cargo.toml` (no mode) → read-write handle; extended syntax `--cap-file cfg='Cargo.toml:[Readable]'` → read-only handle; `narrow` reduces permissions; `narrow` to non-held flag errors (`tests/corpus/eval/`, `tests/corpus/cli/`)

---

## Internal Integrity

### builtin-privacy: Restrict `builtin-*` aliases to prelude evaluation context

Accepted 2026-05-11. See `doc/whatif/builtin-privacy.md`. **Spec chapters:** `doc/11-stdlib.md §Rust-Native vs Tinct-Implemented Boundary`.

- [ ] Migrate `stdlib/macros.llt`: replace all `builtin-*` calls with idiomatic prelude wrappers (`builtin-if` → `if`, `builtin-lt` → `<`, `builtin-add` → `+`, `builtin-get` → `get`, `builtin-reduce` → `reduce`, `builtin-eq` → `=`); run tests to confirm no behavior change (`stdlib/macros.llt`)
- [ ] Migrate `stdlib/path.llt`: replace `builtin-if` → `if`, `builtin-eq` → `=`, `builtin-sub` → `-`, `builtin-add` → `+`, `builtin-get` → `get`; confirm `get` error-on-missing semantics are acceptable for each call site (`stdlib/path.llt`)
- [ ] Migrate `stdlib/toml-lite.llt`: replace all `builtin-*` calls with prelude wrappers; this is the largest migration — `toml-lite.llt` uses nearly every alias; run the TOML corpus tests after migration (`stdlib/toml-lite.llt`)
- [ ] Split `create_root_env()` in `src/builtins.rs`: move `builtin-*` alias registrations out of `create_root_env()` and into a new `inject_prelude_aliases(env)` function; `create_root_env()` returns an env with primary names only (`src/builtins.rs`)
- [ ] Update prelude loading in `src/imports.rs` (`build_prelude_env`): create `prelude_eval_env` = `create_root_env()` + `inject_prelude_aliases()`; evaluate `prelude.llt` in `prelude_eval_env`; the resulting exported bindings become the prelude output env (no `builtin-*` names exposed) (`src/imports.rs`, `src/builtins.rs`)
- [ ] Add type-checker warning `T009` for `builtin-*` references: in name resolution, when the resolved name matches `^builtin-` and the source file is not `prelude.llt`, emit a warning "direct use of internal builtin alias — use the public wrapper instead" (`src/typecheck.rs`)
- [ ] Tests: user code referencing `builtin-lt` → `undefined variable` error; `prelude.llt` still uses `builtin-lt` without error; `--strict` mode with `builtin-*` reference → error; migrated `macros.llt`/`path.llt`/`toml-lite.llt` pass all existing corpus tests (`tests/corpus/eval/`)

---

## Networking

### net-gaps: QUIC datagrams, SPKI correctness, HTTP/3 concurrent driver

Genuine deferred items from the `http-sessions` and `connector-tls` sprints. Each is a deliberate "implement later" stub.

- [ ] Remove `socks5-connect` and `proxy-connect` from `standard_builtins()`, `TypeEnv::with_builtins()`, and the builtin count assertion — decided 2026-05-09 to remove from registry (they return "not yet implemented" errors and SOCKS5 is implemented as a pure-tinct `socks5-layer` in stdlib) (`src/builtins.rs`, `src/builtins_io.rs`, `src/type_env.rs`)
- [ ] Delete stale SPKI comment at `src/builtins_io.rs:3335` — two lines saying "simplified implementation that hashes the whole cert"; `compute_spki_hash` already correctly extracts `subject_pki.raw` (`src/builtins_io.rs`)
- [ ] Add `Value::QuicDatagramHandle(Rc<quinn::Connection>)` variant to the `Value` enum and its `type_name`/`Display`/`PartialEq` impls (`src/value.rs`)
- [ ] Register `Type::QuicDatagramHandle` in `TypeEnv::with_builtins` and add type signature for `quic-open-datagram` (`src/type_env.rs`)
- [ ] Implement `quic-open-datagram`: replace the current "not yet implemented" error body with `block_on(session.open_uni())` to get a send stream; return `Value::QuicDatagramHandle(Rc::clone(&conn))` (`src/builtins_io.rs:4457`)
- [ ] Add `send-datagram` overload for `Value::QuicDatagramHandle`: dispatch to `block_on(conn.send_datagram(bytes))` (`src/builtins_io.rs`)
- [ ] Add `recv-datagram` overload for `Value::QuicDatagramHandle`: dispatch to `block_on(conn.read_datagram())`, return `Bytes` (`src/builtins_io.rs`)
- [ ] Add `async_rt::spawn<F: Future>(fut: F) -> JoinHandle<F::Output>` helper using `TOKIO_RT.with(|rt| rt.spawn(fut))` — tokio `current_thread` runtime drives spawned tasks during `block_on` calls (`src/async_rt.rs`)
- [ ] Define `Http3SessionState { send_request: h3::client::SendRequest<...>, _driver: JoinHandle<()> }` struct in `src/builtins_io.rs`; spawn the h3 `Connection` driver via `async_rt::spawn` and store its `JoinHandle` in the struct to keep it alive
- [ ] Change `Value::Http3Session` to wrap `Rc<RefCell<Http3SessionState>>` instead of the bare `send_request`; update all match arms that destructure it (`src/value.rs`, `src/builtins_io.rs`)
- [ ] Tests: `quic-open-datagram` + `send-datagram` + `recv-datagram` round-trip corpus test; `http3-session` concurrent request (two sequential requests on one session succeed); QUIC datagram type error on wrong handle type (`tests/corpus/eval/`)

---

## LSP

### lsp-gaps: Prelude go-to-definition and remaining LSP quality items

- [ ] **Prelude go-to-definition** (`src/lsp/analysis.rs:802`): Parse the embedded prelude source (`include_str!("../../stdlib/prelude.llt")`) once at LSP startup into a `Spanned<File>` AST and cache it in `DocumentStore`; extend `definition_at()` in `src/lsp/analysis.rs` to search the cached prelude AST using the existing `find_key_definition()` recursion after local/include lookup fails; resolve the prelude URI via `find_libdir_path().join("prelude.llt")` + `file_path_to_uri()` for the `Location` response; `llt_span_to_lsp_range` works unchanged since it takes source text separately from spans (`src/lsp/analysis.rs`, `src/lsp/document.rs`)
- [ ] **`textDocument/documentSymbol`:** walk the top-level dict entries of the current document and return them as `SymbolKind::Variable` symbols with their definition spans; add `document_symbols_at` in `src/lsp/analysis.rs`; register `DocumentSymbolRequest::METHOD` in `src/lsp/server.rs`; declare capability in `ServerCapabilities`; enables IDE outline views and breadcrumbs (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`textDocument/formatting`:** call the existing Rust formatter (`src/formatter.rs`) on the full document source and return a single whole-document `TextEdit`; register `DocumentFormattingRequest::METHOD` in `src/lsp/server.rs`; declare `document_formatting_provider` in `ServerCapabilities`; the formatter already produces a round-tripped source string — wrap it in a diff against the original to produce minimal edits, or return a single replace-all edit for simplicity (`src/lsp/server.rs`, `src/formatter.rs`)
- [ ] **`textDocument/references`:** find all spans in the document where a given name is referenced; add `references_at(doc, offset) -> Vec<Location>` in `src/lsp/analysis.rs` — walk the full AST collecting all `Expr::VarRef` nodes whose name matches the symbol under the cursor; register `References::METHOD` in `src/lsp/server.rs`; declare `references_provider` in `ServerCapabilities` (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`textDocument/rename`:** rename a binding and all its references in the document; reuse `references_at` plus the definition span to produce a `WorkspaceEdit` with `TextEdit` entries for every occurrence; validate the new name is a valid tinct identifier before returning; register `Rename::METHOD` in `src/lsp/server.rs`; declare `rename_provider` in `ServerCapabilities` (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`textDocument/inlayHints`:** return inferred types inline next to unannotated bindings in the visible range; add `inlay_hints_in_range(doc, range) -> Vec<InlayHint>` in `src/lsp/analysis.rs` — for each top-level dict entry whose value is not annotated, look up its inferred `TypeScheme` from the type map and emit a hint with the display string (e.g., `: Int`, `: Fn@Bool [a a]`) positioned after the binding name; register `InlayHintRequest::METHOD` in `src/lsp/server.rs`; declare `inlay_hint_provider` in `ServerCapabilities`; this is the highest-information-density feature for a type-inferred language (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`textDocument/signatureHelp`:** when the cursor is inside a function call `[f ...]`, look up `f`'s `TypeScheme`, extract parameter names and types, and return a `SignatureInformation` showing the full `Fn@Return [param1@Type ...]` signature with the active parameter highlighted based on cursor position; register `SignatureHelpRequest::METHOD` in `src/lsp/server.rs`; declare `signature_help_provider` in `ServerCapabilities` (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`workspace/symbol`:** search all top-level bindings across all open and recently-loaded documents matching a query string; return as `WorkspaceSymbol` entries with their file URIs and definition ranges; register `WorkspaceSymbolRequest::METHOD` in `src/lsp/server.rs`; declare `workspace_symbol_provider` in `ServerCapabilities` (`src/lsp/server.rs`, `src/lsp/document.rs`)
- [ ] **Hover: show inferred type alongside declared annotation when they differ** (`src/lsp/analysis.rs`): when a binding has an explicit annotation and the inferred type from `type_map`/`scheme_map` is strictly narrower (e.g., declared `@Number` but inferred `Int`, declared `@Dict` but inferred `{name: String}`), append the inferred type to the hover: `"x (Number) — inferred: Int"`; use `is_subtype(inferred, declared) && !is_subtype(declared, inferred)` to detect the "narrower" case; no change needed when they match or when there is no annotation (`src/lsp/analysis.rs`, `src/types.rs`)
- [ ] [Major] Verify LSP `document.rs` `update_document` calls `desugar_file()` BEFORE `typecheck_file()` — all other entry points follow `expand_macros → desugar → resolve → typecheck → eval`; if LSP reorders or skips desugar, the type checker sees `VarRef("_")` instead of desugared `Fn` nodes producing spurious "undefined variable _" errors; confirm and add a PIPELINE INVARIANT comment (`src/lsp/document.rs`)

---

## Evaluator and Macros

### eval-gaps: Unquote nesting, error span threading

Two correctness/quality gaps in the evaluator noted in source comments.

- [ ] **Unquote in nested positions** (`src/eval.rs:1343`): The `eval_quote` fallback arm (`_ =>`) calls `ast_to_dict_expr` which does not recognize `Expr::Unquote`/`Expr::UnquoteSplice` in nested positions; add a recursive `eval_quote_expr` pre-pass in `src/eval.rs` that walks the full `Expr` tree — when it encounters `Expr::Unquote(inner)`, evaluate `inner` and substitute the result as a serialized AST value node; when it encounters `Expr::UnquoteSplice(inner)` in a list position, splice the evaluated sequence; all other nodes recurse unchanged; replace the `_ =>` arm with a call to `eval_quote_expr` then `ast_to_dict_expr`; `ast_to_dict_expr` is unchanged (`src/eval.rs`); add corpus tests for nested `[unquote ...]` inside call args, dict values, and seq literals (`tests/corpus/eval/`)
- [ ] Remove stale `#[allow(dead_code)]` attribute on `eagerly_register_constructors` in `src/eval.rs:1261` — the function is actively called from `src/eval_dict.rs` and the lint fires spuriously for `pub(crate)` items in some configurations (`src/eval.rs`)
- [ ] Fix stale test comment at `src/typecheck.rs:8200` that says "`@[...]` composite annotation is not yet implemented in the parser" — `Annotation::PropertyDict` is fully implemented and used throughout the prelude; update the comment (`src/typecheck.rs`)
- [ ] Make TypeAssert materialization iterative: replace the `eval_recursive` call at `src/eval_materialize.rs:1655` (`TODO(cek-eval)`) with a `TypeAssertCheck` continuation — push the check onto the continuation stack and use `Action::Eval` for the inner expression instead of recursing (`src/eval_materialize.rs`)
- [ ] **`mat_span` threading through DotAccessForceData** (`src/eval_materialize.rs:1344`, `src/eval_materialize.rs:1379`): When `.field` access in an access chain triggers materialization, the `mat_span` used is the access expression span rather than the outer materialization context's span — this loses the outermost call-site span in error messages for chained access like `a.b.c`; fix by threading `outer_mat_span: Option<Span>` through `DotAccessForceData` and using it in `Action::Materialize`; corresponding test is at `src/eval.rs:5559` (currently asserts the wrong span as a known limitation — update when fixed)

---

## Tooling

### tinct-hosted-formatter: Implement stdlib/formatter/format.llt

Accepted 2026-05-05. See `doc/whatif/completed/tinct-hosted-formatter.md` for the full design.
The Rust formatter (`src/formatter.rs`) is retained for LSP use; this formatter receives the AST dict from `ast_to_dict` and returns formatted source as a tinct string.

- [ ] Implement `stdlib/formatter/compact.llt` and `stdlib/formatter/pretty.llt` as tinct programs that receive `%` as the AST dict (from `ast_to_dict(Some(src), Some(comments))`) and return formatted source; wire `tinct fmt --compact`/`--pretty` to invoke these via the evaluator
- [ ] Implement `stdlib/formatter/format.llt` as the full formatter — layout algorithm, indentation, comment attachment, multi-line decisions per `doc/whatif/completed/tinct-hosted-formatter.md`; wire to `tinct fmt` (default mode)
- [ ] The Rust formatter (`src/formatter.rs`) is retained for LSP use — add a `FormatterMode` enum to dispatch between Rust and tinct-hosted based on invocation context; LSP always uses Rust formatter
- [ ] Tests: round-trip corpus tests (format → re-parse → compare AST); test compact/pretty/full modes; test comment preservation

### doc-weave-result-substitution: Document pipeline result substitution

- [ ] **Document result substitution** (`doc/09-documents.md:953`): Implement `weave` mode inline result marker replacement — after evaluating each tinct code block, replace the trailing `<!-- tinct-result: ... -->` HTML comment in the Markdown with the block's JSON output; currently these markers are inserted but never updated on re-run; requires threading the Markdown source through `weave` output generation and scanning for marker positions
