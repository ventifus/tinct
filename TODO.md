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

### prelude-annotation-sweep: Comprehensive annotation pass over all public prelude functions

Audit every public-facing function in `stdlib/prelude.llt` (excluding internal helpers: names ending in `-impl`, `-step`, `-check`, `-merge`, and `sort-merge`) and apply precise annotations using the `fn@[return: ... constraint: ... doc: ...]` form where the existing annotation is imprecise or missing.

**What to look for:** `fn@Unknown` or bare `fn` where:
- The return type is determinable from the body or parameters (e.g., a TypeVar tied to an input)
- A constraint is inferrable from operators used in the body (e.g., body calls `<` → `Comparable a`)
- A `doc:` string would materially help LSP users understand the function

**What not to change:** `fn@Type` where the shorthand is already precise; `fn@Unknown` where the return genuinely cannot be typed without HKT or multi-parameter type classes (e.g., `zip` — deferred to `hkt-mappable-appendable`).

- [ ] Scan all public functions in `stdlib/prelude.llt` for `fn@Unknown` and bare `fn` (no annotation); use `grep -n 'fn@Unknown\|: \[fn ' stdlib/prelude.llt` as starting point; for each, determine the correct annotation from the function body (`stdlib/prelude.llt`)
- [ ] Apply TypeVar-return annotations for accumulator-pattern functions: `reduce`/`fold` → `fn@a [f@Fn init@a xs]`; `group-by` (if present) → appropriate TypeVar (`stdlib/prelude.llt`)
- [ ] Apply constraint annotations for comparison/sorting functions not already covered by `hkt-doc-lsp`: any function using `<`, `>`, `<=`, `>=` on a polymorphic arg gets `constraint: [a: Comparable]`; functions using `=` get `constraint: [a: Equatable]` (`stdlib/prelude.llt`) — **gate on `constraint-annotations` sprint**
- [ ] Apply union-return TypeVar annotations: functions returning either their input type or a sentinel (e.g., `Null`/empty) get `fn@[a Null]` or `fn@[a Record]` where `a` matches a parameter TypeVar — covers any remaining functions missed by batch B (`stdlib/prelude.llt`)
- [ ] Add `doc:` strings to all public functions currently lacking one — focus on: string utilities (`pad-left`, `pad-right`, `words`, `lines`), math utilities (`clamp`, `abs`, `sign`), structural utilities (`flatten`, `zip-with`, `group-by`), and any function whose name alone is not self-documenting (`stdlib/prelude.llt`)
- [ ] For functions whose precise type requires HKT (e.g., `zip` dual-dispatch, higher-order combinators) — add `doc:` string describing the type but leave `fn@Unknown` until `hkt-mappable-appendable` lands; add a comment `# annotation deferred to hkt-mappable-appendable` (`stdlib/prelude.llt`)
- [ ] Run full corpus test suite after all annotation changes to confirm no regressions (`just test` or equivalent); fix any annotation that causes a type error by reverting to `fn@Unknown` and filing a note

**Depends on:** `constraint-annotations`

### typecheck-precision: Type::Error sentinel and pure-sequence builtin types

Implements the Precision tier of the Type System Extension Roadmap (`doc/07-type-extensions.md §Type System Extension Roadmap`). Design is complete in that section — this sprint is implementation only.

- [ ] Add `Type::Error` sentinel variant to `src/types.rs`; update all exhaustive `Type` match sites with arms that propagate `Error` silently (`src/types.rs`, `src/type_unify.rs`, `src/typecheck.rs`); semantics: `unify(Error, τ) = S` unchanged (no new binding, no error propagation), `is_subtype(Error, _) = false`
- [ ] In `infer_expr` and `check_expr`, when a subexpression produces `TypeError`, bind the expression's type to `Type::Error` in the type map rather than propagating the error to all dependents; subsequent errors on `Type::Error`-typed subexpressions are suppressed (`src/typecheck.rs`)
- [ ] Register precise return types for pure-sequence builtins in `TypeEnv::with_builtins()`: `$range → Seq(Int)`, `$seq: (T, Int) → Seq(T)`, `$repeat: (T) → Seq(T)`, `$cycle: (Seq(T)) → Seq(T)`, `$take: (Int, Seq(T)) → Seq(T)`, `$drop: (Int, Seq(T)) → Seq(T)` — dual-dispatch `$map` and `$filter` remain `Unknown` until `hkt-mappable-appendable` (`src/type_env.rs`)
- [ ] Update LSP hover: display "error" for `Type::Error`-typed bindings rather than nothing; suppress hover for expressions whose type is `Error` due to cascading from an upstream error (`src/lsp/analysis.rs`)
- [ ] Tests: a single type error does not produce N cascading errors on dependent expressions; `$range 0 10` types as `Seq(Int)` in LSP hover; `$repeat "a"` types as `Seq(Str)`; LSP shows "error" for error-typed binding (`tests/corpus/eval/typecheck/`, `tests/lsp_corpus_tests.rs`)

### typecheck-completeness: Polymorphic recursion ban and CALL-MONO/CALL-POLY fix

Implements the Completeness tier of the Type System Extension Roadmap (`doc/07-type-extensions.md §Type System Extension Roadmap`). Both designs are specified in that section.

- [ ] **Polymorphic recursion ban:** in `check_call`, detect when a recursive call site instantiates a TypeVar that was bound by an outer call to the same function (depth limit: 1 — immediate rejection); emit error "polymorphic recursion requires an explicit type annotation — annotate the function's return type with `fn@T`" with a help span pointing to the definition; add corpus test: unannotated self-recursive call that would diverge during inference → clear error (`src/typecheck.rs`, `tests/corpus/eval/typecheck/`)
- [ ] **CALL-MONO/CALL-POLY divergence fix:** replace the dual-path design (unify for CALL-POLY, `is_subtype` for CALL-MONO) with a single structural `check_expr` pass that applies [SUB] at leaves and unification only at actual TypeVar positions; eliminates the case where identical literal pairs produce different verdicts depending on whether TypeVars were present — see `doc/07-type-extensions.md §Completeness` for the exact description (`src/typecheck.rs`)
- [ ] Tests: CALL-MONO and CALL-POLY agree on all literal type pairs; recursive function with annotation works; recursive function without annotation that would polyrecurse → error (`tests/corpus/eval/typecheck/`)

### typeassert-convergence: Structural TypeAssert runtime validation

Design in `doc/07-type-extensions.md §TypeAssert Runtime Validation`. Closes the static/runtime divergence: static check uses `is_subtype` but runtime uses nominal `value.type_name()` string comparison, making record-type assertions no-ops at runtime.

- [ ] Extend `Expr::TypeAssert` in `src/ast.rs`: add `resolved_type: RefCell<Option<Type>>` field initialized to `None` by the parser; type checker populates it via `resolve_type_assert()` with the fully-substituted concrete type (type aliases resolved at check time — evaluator never resolves aliases) (`src/ast.rs`, `src/typecheck.rs`)
- [ ] Evaluator: when `resolved_type` is `Some(ty)`, use `ty` for validation instead of `value.type_name()` string comparison; for primitive types (Int, Str, Bool, Float, Null, Seq), validate immediately; for `Record(fields, Closed)`, check key set + cardinality immediately, then create `Guarded` thunks for each field's type constraint (field type checked lazily at first access, preserving lazy semantics — Findler & Felleisen 2002); when `resolved_type` is `None` (`--no-typecheck` mode), degrade to current nominal behavior (`src/eval.rs`)
- [ ] Add `Cont::TypeAssertCheck(Type, Span)` continuation: fired when a Guarded thunk for a record TypeAssert field is first materialized; validates `value_matches_type(actual, field_ty)`, produces `TypeAssertFailed` on mismatch with the TypeAssert span as primary location (`src/eval.rs`)
- [ ] Tests: `[@[name: String] {name: "alice"}]` passes; `[@[name: String] {name: 42}]` → `TypeAssertFailed` at first access of `name`; `[@Int "hello"]` → immediate failure; `[@Unknown expr]` → always passes; `--no-typecheck` → nominal fallback; nested record assert validates inner structure (`tests/corpus/eval/typeassert/`)

### stdlib-stack-frames: Stack frame filtering for stdlib error locations

Limitation documented in `doc/11-stdlib.md §Known Limitations`. Stdlib functions that call `$error` internally show `prelude.llt` as the primary error location rather than the user's call site.

- [ ] Add stdlib-frame classifier to error rendering in `src/error.rs`: a frame is "stdlib internal" when its source URI matches the prelude path (`find_libdir_path().join("prelude.llt")`) AND its function name carries a known internal suffix (`-impl`, `-step`, `-check`, `-merge`); internal frames are demoted to a secondary "from stdlib" note (`src/error.rs`)
- [ ] Identify the user call-site frame: the outermost non-internal frame in the `eval_stack` becomes the primary error location; if all frames are stdlib-internal, fall back to innermost stdlib frame (current behavior unchanged) (`src/error.rs`, `src/eval.rs`)
- [ ] Tests: `$error` called inside a stdlib function named with `-impl` suffix → user call site is primary error location; builtin that calls `$error` directly (not via prelude.llt) → unaffected; pure user code calling `$error` → unaffected (`tests/corpus/eval/errors/`)

---

## Higher-Kinded Types

Accepted 2026-05-11. See `doc/whatif/completed/hkt-monads.md` for the full design.
Adds `Kind::Operator` (`* → *`), `Kind::Label`, `Type::App`/`Type::Operator`, the Functor/Applicative/Monad/Foldable/Traversable/Mappable/Appendable typeclass hierarchy, Maybe ADT, `HasField` qualified-type constraint for precise `get`/`get-in` typing, generic functions (sequence, traverse, forM, when, liftM2), and inferred `[do]`.

### hkt-foundation-a: Core type constructs — Kind, App/Operator, UNIFY rules

See `doc/whatif/completed/hkt-monads.md` §Kind System, §Type Constructor Application. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §Formal Type Rules`.

- [ ] Add `Kind::Operator` (as `Kind::Arrow(Box::new(Kind::Type), Box::new(Kind::Type))`) and `Kind::Label` variants to `Kind` enum (`src/types.rs`); display `Operator` as `"* → *"`, `Label` as `"Label"`
- [ ] Add `Type::App(Box<Type>, Box<Type>)` and `Type::Operator(String)` variants; implement `PartialOrd`/`Ord` consistent with `normalize_union` sort order (`src/types.rs`)
- [ ] Update ALL exhaustive `Type` match sites for `App`/`Operator`: `src/desugar.rs` (`_ => unreachable!()`), `src/eval.rs` main match (`EvalError::internal`) + `value_matches_type` (`App|Operator => true`), `src/typecheck.rs` (inference arms), `src/lsp/analysis.rs` (hover + Expr::TypeApp), `src/ast_dict.rs` (both directions), `src/type_unify.rs` (`unify`, `is_subtype` placeholder arms), `src/type_env.rs` Display
- [ ] Update `Type` tree-walker functions in `src/type_unify.rs` — **required before UNIFY-APP/UNIFY-OPERATOR**: `collect_all_vars`, `collect_all_vars_check_occurs`, `collect_all_vars_vec`, `has_inference_vars`, `lower_levels_check_occurs` — add `App`/`Operator` arms recursing into both sub-types
- [ ] Update `Substitution::apply_type`: add `App(f, a)` arm (recurse into both) and `Operator(m)` arm (look up in substitution map) (`src/type_unify.rs`)
- [ ] Add `Display` for `Type::App` as `"[{f} {a}]"` and `Type::Operator(name)` as `"{name}"` (`src/type_env.rs`)
- [ ] Add `UNIFY-OPERATOR`: `unify(Operator(m), T) = [m ↦ T]` with occurs check (m must not appear anywhere inside T, including inside `App` sub-expressions) + `kind_env ⊢ T : *` premise (prevents binding to Operator/Label-kinded types); symmetric `unify(T, Operator(m)) = [m ↦ T]`; also add `UNIFY-OPERATOR-SYM`: `unify(Operator(m), Operator(n))` where `m ≠ n` → `[m ↦ Operator(n)]` (bind one Operator var to another — needed when unifying two class params at an instance resolution site) (`src/type_unify.rs`)
- [ ] Add `UNIFY-APP`: unify constructors first, apply substitution, unify arguments; return composed substitution (`src/type_unify.rs`)
- [ ] Add `Expr::TypeApp(Box<Expr>, Box<Expr>)` to `src/ast.rs`; add eval handler returning `EvalError::internal` — annotation-only node (`src/eval.rs`); extend `src/parser.rs` annotation parsing: when a `[...]` annotation in type annotation position contains no `:` entries AND the first element resolves to an Operator-kinded var (checked in the type checker, not at parse time), emit `Expr::TypeApp(f, a)` rather than treating as a union type; add disambiguation corpus test for `@[Int Null]` (two positional elements, first is concrete type → union, not TypeApp)
- [ ] Recognize `@Operator` annotation: emit `TypeError` "Operator is a kind, not a type — annotate a class type parameter as `f@Operator`, not a value expression" (`src/typecheck_annot.rs` `resolve_type_name`)
- [ ] Update ALL exhaustive `Type` match sites — note this includes `src/typecheck_annot.rs` (add `App|Operator` arms in `resolve_type_expr` and `resolve_type_name`); initial stub arms are sufficient in this sprint (quality improvement in `hkt-doc-lsp`)
- [ ] Add `src/lsp/analysis.rs` initial stub match arms for `Type::App`/`Type::Operator` in hover (display as type string) and `Expr::TypeApp` in expression matches (quality improvement in `hkt-doc-lsp`)
- [ ] Tests: `Type::App` display `[Result Int]`, UNIFY-APP/UNIFY-OPERATOR/UNIFY-OPERATOR-SYM unit tests, TypeApp eval handler error, `@Operator` annotation error, disambiguation test for `@[Int Null]` = union not TypeApp (`src/type_unify.rs`, `tests/corpus/eval/typecheck/`)

### hkt-foundation-b: Class/label infrastructure — kind_env, Label ADT, ClassDecl

See `doc/whatif/completed/hkt-monads.md` §Kind System §Kind::Label, §What Would Change. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §Formal Type Rules`.

- [ ] Add `pub enum Label { Concrete(String), Var(String) }` to `src/types.rs` (or `src/type_unify.rs`) — used exclusively in `HasField` constraint's label position
- [ ] Add `kind_env: HashMap<String, Kind>` to `InferState` (`src/types.rs`); populate `Kind::Operator` during class method signature processing; populate `Kind::Label` when `key@"k"` annotation is resolved
- [ ] Extend `class` declaration parsing: change `Expr::ClassDecl.superclasses` from `Vec<String>` → `Vec<(String, String)>`; update ALL match sites: `src/expand.rs`, `src/ast_dict.rs` (both), `src/formatter.rs` (**must** emit `extends [SuperClass param]` — currently drops them), `src/typecheck.rs`, `src/eval.rs`, `src/parser.rs`
- [ ] Update `ClassDecl` construction in `src/typecheck.rs`: assign `Kind::Operator` when class parameter is annotated `@Operator` or constrained by an Operator-kinded class
- [ ] In `src/typecheck_annot.rs` `resolve_type_expr`: parse `[f a]` (no colons) as `Expr::TypeApp(f, a)` when `f` is Operator-kinded in `kind_env` or a user parameterized alias; builtins keep `@Seq@T` path
- [ ] In `src/typecheck_annot.rs` `resolve_type_name`: when annotation value is a string literal (e.g. `key@"k"`), create fresh TypeVar, register `kind_env[fresh] = Kind::Label`, bind the annotation name to it
- [ ] Extend `promote_literal_for_constrained_var` (`src/type_unify.rs`): the fix is a **single change to the function body** (line ~170) — after the "no constraints" early return, insert `if state.kind_env.get(var_name) == Some(&Kind::Label) { return ty; }` BEFORE the `match ty` literal-widening arms; this single change covers all 6 call sites automatically
- [ ] Add `label_vars: Vec<String>` to `TypeScheme` (`src/types.rs`) — add AFTER `constraint-annotations` has already added `doc: Option<String>` (do not remove the `doc` field); update `instantiate_scheme` to register freshly-instantiated label vars in `state.kind_env` with `Kind::Label`
- [ ] Enforce `KIND-LABEL-ERROR` in kind pre-pass (`src/typecheck.rs`): reject `Seq(TypeVar(l))`, `App(_, TypeVar(l))` etc. when `kind_env(l) = Kind::Label`; emit `TypeError` "label variable cannot appear as a type"
- [ ] Tests: `key@"k"` creates label TypeVar, `promote_literal` skips for `Kind::Label`, kind error for `Seq(label_var)`, label_vars survive generalization and re-register at call sites (`tests/corpus/eval/typecheck/`)

**Depends on:** `hkt-foundation-a`

### hkt-field-access: HasField constraint, typed get/get-in

See `doc/whatif/completed/hkt-monads.md` §Field Access Typing. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §Formal Type Rules §Field Access Typing`.

- [ ] Migrate `Constraint` from `{ class: String, var: String }` struct to `pub enum Constraint { Class { class: String, var: String }, HasField { label: Label, dict_var: String, field_var: String } }` (`src/types.rs`); `label: Label` uses the `Label` ADT from `hkt-foundation-b` (`pub enum Label { Concrete(String), Var(String) }`); update ALL Constraint creation sites to `Constraint::Class { class, var }` — search: `grep -n 'Constraint {' src/` (expected in `src/typecheck_annot.rs`, `src/type_unify.rs`, `src/typecheck.rs`, `src/type_env.rs`); update all match sites on `Constraint`
- [ ] Add `resolve_has_field(label, dict_type, field_var, state)` in `src/type_unify.rs` implementing `[HAS-FIELD-REC]`, `[HAS-FIELD-UNION]`, `[HAS-FIELD-INTER]`, `[HAS-FIELD-TOP]` (for S-RcdTop-collapsed unions), `[HAS-FIELD-UNKNOWN]`, `[HAS-FIELD-NEVER]`; uninhabitable-intersection warning; field-set merge accumulation; **BAS ordering**: in `check_get` (`src/typecheck.rs` line 2125), resolve `HasField` constraints on the inferred `dict_ty` BEFORE calling `Type::normalize_union` or `Type::simplify_type` on the dict type — the union must be processed in its un-normalized form so `[HAS-FIELD-UNION]` can fire before S-RcdTop collapses disjoint-field unions to ⊤
- [ ] Extend `check_get` in `src/typecheck.rs` (line 2125): add TypeVar arm — only when `kind_env[key_var] = Kind::Label` (key is a label TypeVar) emit `Constraint::HasField { label: Label::Var(name), ... }`; for unannotated TypeVar keys (not label-kinded), fall back to `Unknown` (existing behavior); add Union arm (`[HAS-FIELD-UNION]`), Intersection, Top/Unknown arms; deferred constraints in `state.constraints` as `Constraint::HasField`, merged by field-set union (not structural record unification)
- [ ] Add `check_get_in` in `src/typecheck.rs` — called as special-form handler receiving raw AST node; unfolds syntactic `Seq` literal paths where all elements are `StringLiteral` via `[GET-IN-CONS]`; falls back to `Unknown` for variable-length or non-literal paths
- [ ] Register `get`'s label-polymorphic scheme in `src/type_env.rs`: `∀ (l:Label) d a. HasField l d a => StringLiteral(l) → d → a`; register `get-in` as special form dispatched to `check_get_in`
- [ ] Update `stdlib/prelude.llt` `get`/`get-or`/`get-in` annotations (requires `constraint-annotations` sprint to have landed):
  ```tinct
  get:    [fn@[return: a  constraint: [HasField l d a]] [key@"l"  dict@d] ...]
  get-or: [fn@[return: a  constraint: [HasField l d a]] [key@"l"  dict@d  default@a] ...]
  get-in: [fn@[doc: "Chained field access — return type inferred from literal path"] [path  dict] ...]
  ```
- [ ] Tests: `[get "name" {name: String}]` → `String`; TypeVar dict → field constraint generated; `[get "name" (A|B)]` → `A.name|B.name`; `[get k dict]` with `k:Str` → `Unknown`; `[get-in ["a" "b"] nested]` → field type; variable path → `Unknown`; label-polymorphic fn inferred type; conflicting intersection warns (`tests/corpus/eval/typecheck/`, `=== out`/`=== error`)

**Depends on:** `hkt-foundation-b`, `constraint-annotations`

Note: `hkt-field-access` also requires `constraint-annotations` because the prelude annotation task uses `fn@[constraint: [HasField l d a]]` syntax. This is captured in the `Depends on:` line above.

### hkt-bas: BAS extension for App type atoms and functorial subtyping

See `doc/whatif/completed/hkt-monads.md` §Interaction with BAS. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §Interaction with BAS`.

- [ ] Extend `is_subtype` in `src/types.rs`: add arm `(App(f₁, a), App(f₂, b))` — when `f₁ == f₂` and `is_subtype(a, b)`, the application is a subtype; restricted to known-covariant stdlib instances; no `ClassEnv` access needed
- [ ] Verify `App(m, a) | App(m, b) <: App(m, a|b)` is derived automatically via UNION-ELIM + covariance — confirm via tests, no separate rule needed
- [ ] Do NOT implement the reverse direction `App(m, a|b) <: App(m, a) | App(m, b)` — unsound for diagonal functors
- [ ] Verify tree-walkers/apply_type handle `App`/`Operator` correctly (updated in `hkt-foundation-a`); run all BAS corpus tests to confirm no regressions
- [ ] Tests: `App(Result, Int) <: App(Result, Int|Str)` (covariance), join via UNION-ELIM, reverse direction NOT accepted, mismatched constructors rejected (`tests/corpus/eval/typecheck/`)

**Depends on:** `hkt-foundation-a`

### hkt-kind-inference: Kind checking pass and Operator-kinded class resolution

See `doc/whatif/completed/hkt-monads.md` §Kind Checking, §Typeclass Resolution for HKT. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §Formal Type Rules`.

- [ ] Add kind inference pre-pass in `src/typecheck.rs`: walk class method signatures, look up parameter kinds from `kind_env`; assign `Kind::Operator` to parameters annotated `@Operator` or constrained by an Operator-kinded class
- [ ] Implement `KIND-OPERATOR` validation: `App(f, a)` during annotation resolution — `f : Operator`, `a : *` → valid; `f : *` → `TypeError` "kind mismatch: expected `* → *`, got concrete type" (`src/typecheck.rs`, `src/typecheck_annot.rs`)
- [ ] Enforce rank-1 restriction: reject `App(Operator("f"), Operator("g"))` (both Operator-kinded) — emit `TypeError` "rank-2 type constructor application is not supported"; note that multiple flat Operator vars in a single method type (like `traverse`'s `f` and `t`) are correctly rank-1 and NOT rejected (`src/typecheck.rs`)
- [ ] Extend `ClassEnv` lookup for Operator-kinded class params: unify instance head against `App(m, _)` using UNIFY-APP (`src/type_env.rs`); the `resolve_instance` freshening fix (freshen free type vars via `instantiate_at_level`, capture not discard `temp_subst`) is **implemented in `hkt-mappable-appendable`** where it is first needed for `AppendableSeq [Seq b]`; this sprint's task is to wire up the Operator-kinded lookup path, not to implement the freshening
- [ ] Add `App` type inference: when binding infers `App(Operator("m"), a)`, apply UNIFY-OPERATOR against known instance heads; update `InferState.subst` (`src/typecheck.rs`)
- [ ] Normalize at instance resolution: `App(Seq_ctor, T) → Type::Seq(T)`; `App(App(Map_ctor, K), V) → Type::Map(K, V)`; `App(Result_ctor, T)` stays as `App` (`src/typecheck_annot.rs`)
- [ ] Assign error code `E091` for kind mismatch errors in `src/error.rs`; add to `doc/10-errors.md` all three tables (variant catalog, codes table, categories table) — `hkt-doc-lsp` will verify these entries exist, not re-add them
- [ ] Tests: kind mismatch errors with `[E091]` prefix, `App(Result, Int)` inferred from `[Ok 42]`, rank-1 violation rejected (but multiple flat Operator vars in one method type like `traverse` are NOT rejected), Operator-kinded class constraint resolution (`tests/corpus/eval/typecheck/`)

**Depends on:** `hkt-foundation-b`

### hkt-do-macro: Implement [do] macro — explicit form first, inferred form second

See `doc/whatif/completed/hkt-monads.md` §`[do]` Inference. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §[do] Inference`.

Note: The explicit `[do monad steps...]` desugaring has no HKT dependency and can proceed after `hkt-foundation-b` (it needs the ClassEnv for monad dict dispatch setup, but not the full kind inference pass).

- [ ] Implement explicit `[do monad steps...]` desugaring in `stdlib/macros.llt` (or `stdlib/prelude.llt` if `stdlib-defmacro` has already landed and merged macros.llt away): classify each step as binding (`[x: expr]`) or non-binding by inspecting the AST dict shape; bindings → `[monad.bind expr [fn [x] ...]]`; non-bindings → `[monad.bind expr [fn [_] ...]]`; `[do monad]` with no steps → `[monad.pure []]`; `[do]` with zero args → error
- [ ] Add `expected_return: Option<Type>` field to `InferState` (`src/types.rs`) — set by `infer_fn` before descending into the function body when the function has an explicit return type annotation; used by the `[do]` inferred form resolution; using `InferState` (not a parameter) avoids a cascading `infer_expr` signature change
- [ ] Implement inferred `[do steps...]` form: emit `[do %do-infer steps...]` sentinel (`Expr::VarRef("%do-infer")`) at macro-expand time; in `src/typecheck.rs` `infer_expr`, when the `[do]` form has `%do-infer` as its monad, resolve sentinel via: (1) `state.expected_return` unifying with `App(m, _)` for a registered Monad; (2) first binding RHS type `App(m, a)` for a known Monad; (3) if unresolved, emit error; the runtime always sees `[monad.bind ...]` with a concrete dict (inferred form substitutes the resolved monad name before eval)
- [ ] Emit "cannot infer monad for `[do]` — add an explicit monad argument or annotate the enclosing function's return type"
- [ ] Tests: `[do result ...]` three-step success, `[Err "fail"]` propagation (short-circuit), explicit `[do]` with any `bind:`-carrying dict (backward compat), inferred `[do]` from `@Result` annotation, inferred from first binding type, missing-monad error, `[do monad]` with no steps → `[monad.pure []]` (`tests/corpus/eval/`)

**Depends on:** `hkt-foundation-b` (explicit form); `hkt-kind-inference` (inferred form)

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

**Depends on:** `hkt-kind-inference`, `constraint-annotations`

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

**Depends on:** `hkt-stdlib`, `hkt-bas`, `constraint-annotations`

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

---

## Syntax

### multi-line-strings: `unindent` stdlib function and `"""` macro

Accepted 2026-05-11. See `doc/whatif/multi-line-strings.md`. **Spec chapters:** `doc/02-syntax.md §2.3.6 Multi-Line Strings`, `doc/11-stdlib.md §Strings`. No lexer changes needed — literal newlines in `"..."` already work. `"""` is a parse-stage macro wrapping `[unindent "..."]`.

- [ ] Add `unindent` to `stdlib/prelude.llt`: use sequential fn body — binding dict `[ls: [lines s]  n: [length [last ls]]  inner: [slice 1 -1 ls]]` followed by `[join "\n" [map [fn [l] [slice n [length l] l]] inner]]`; the binding dict's entries are in scope for the final expression via `Expr::Sequential` (`stdlib/prelude.llt`)
- [ ] Register `"""` and `i"""` as parse-stage macros in `stdlib/macros.llt`: `"""content"""` → `[unindent "content"]`, `i"""content"""` → `[unindent i"content"]`; the lexer already tokenizes the content correctly (`stdlib/macros.llt`)
- [ ] Add note to `doc/02-syntax.md §String Literals` that `"..."` permits embedded literal newlines; document `"""..."""` and `i"""..."""` as the idiomatic indentation-stripping form; document `unindent` as the underlying function (`doc/02-syntax.md`)
- [ ] Tests: `unindent` directly on a raw indented string, `"""..."""` value matches `[unindent "..."]`, `i"""..."""` with `$var` interpolation, single `"` inside triple-quoted content, empty lines preserved, `[trim [unindent ...]]` trailing-newline suppression (`tests/corpus/eval/`)

### seq-lazy-bindings: Sequential let-binding values should be lazy thunks

`Expr::Sequential` in `[fn ...]` bodies (and wherever else `eval.rs` processes intermediate dict expressions) forces each named binding's value to WHNF before inserting it into the child scope (`eval.rs:687-690`). Only the **key** needs to be known for scope-chain construction — the value should remain a lazy thunk, consistent with every other binding in the language. The current "strict let\*" behavior means a dead binding `[x: [error "boom"]] 42` fails instead of evaluating to `42`, contradicting LLT's laziness model. `doc/08-evaluation.md:899` incorrectly states this is "inherent" to scope chain construction. **Spec chapters:** `doc/08-evaluation.md §Strictness Exceptions §4 SEQ-SCOPE`, `doc/08-evaluation.md §Laziness Design` table row 997.

- [ ] In `src/eval.rs` `Expr::Sequential` arm (lines 683-693): replace the `materialize` + `Thunk::new_materialized` block with a direct insert of `val_thunk`; the dict structure (keys) is already known from the `materialize` at line 652-653 — no additional forcing of values is needed (`src/eval.rs`)
- [ ] Update `doc/08-evaluation.md §Strictness Exceptions` item 4 (SEQ-SCOPE): remove "shallowly materialized" and "strict let\*" language; replace with "named binding values are inserted as lazy thunks — only keys must be known for scope chain construction" (`doc/08-evaluation.md`)
- [ ] Update `doc/08-evaluation.md` laziness table row for "Document scope chain (`eval_document`)" / sequential bindings: correct "materialized eagerly (strict let\*)" to "keys extracted eagerly; values remain lazy thunks" (`doc/08-evaluation.md`)
- [ ] Scan corpus tests for any test that expects a dead sequential binding to fail eagerly (pattern: `[x: [error ...]] result` expecting error rather than `result`); update those tests to expect lazy behavior (`tests/corpus/eval/`)
- [ ] Add corpus test: `[fn [] [x: [error "dead"]] 42]` evaluates to `42` (dead binding never forced); `[fn [] [x: [error "live"]] x]` fails with "live" (live binding forced at use site) (`tests/corpus/eval/`)

### multi-body-positions: Extend sequential multi-body to match arms and macro bodies

`Expr::Sequential` (multi-body let-binding) already works in `[fn ...]` bodies with no evaluator or type-checker changes needed. Extend the same rule to other body positions: wherever the parser has a natural delimiter after which it reads expressions until `]`, allow multiple expressions and wrap them in `Expr::Sequential`. No new keywords. **Spec chapters:** `doc/02-syntax.md §2.3.2 Special Forms`, `doc/04-functions.md`.

- [ ] Extend `[match ...]` arm parsing in `src/parser.rs`: after each arm's pattern, read expressions greedily until the next pattern-looking entry (a bracket starting with a pattern) or the closing `]`; if more than one expression, wrap in `Expr::Sequential`; the existing sequential semantics (intermediate dicts extend scope, last expr is result) apply unchanged (`src/parser.rs`)
- [ ] Extend `[defmacro ...]` body parsing in `src/parser.rs`: after the param list `[...]`, read remaining expressions as a body sequence; if more than one, wrap in `Expr::Sequential`; same treatment as `[fn ...]` bodies today (`src/parser.rs`)
- [ ] Update `src/formatter.rs`: when a match arm body is `Expr::Sequential`, format its expressions indented on separate lines (same as fn multi-body formatting) (`src/formatter.rs`)
- [ ] Update `doc/02-syntax.md` and `doc/04-functions.md`: document that `[match ...]` arm bodies and `[defmacro ...]` bodies accept multiple sequential expressions; clarify that `[if ...]` branches and call arguments do not (no body delimiter) (`doc/02-syntax.md`, `doc/04-functions.md`)
- [ ] Tests: match arm with binding dict + result expression, nested match arm multi-body, defmacro with multi-body, formatter round-trip of multi-body match arm (`tests/corpus/eval/`, `tests/corpus/format/`)

### docgen-bugs: Fix bugs discovered during docgen.llt per-module refactor

Three concrete issues surfaced during the `just docgen` refactor.

- [ ] **`replace` arity mismatch in type checker:** `replace` is a 3-arg builtin (`pattern replacement input`) but `TypeEnv::with_builtins()` registers it as 2-arg; `[replace "/" "-" s]` type-checks as an arity error while working fine at runtime; fix the registration in `src/type_env.rs` to match `builtin_replace`'s actual 3-arg signature (`src/type_env.rs`) — tracked as `builtin-type-audit` gap
- [ ] **`write` return value documented as null but returns `{}`:** `builtins_io.rs:1465` returns `Value::Dict(IndexMap::new())` but the doc comment says "returns null"; either fix the return to `ok_val(Value::Null, call_span)` or update the doc comment to say "empty dict `{}`"; the distinction matters for callers using the return value in boolean context (`src/builtins_io.rs:1401`, `src/builtins_io.rs:1465`)
- [ ] **Document the `[= w w]` force-side-effect idiom:** tinct has no `!` or `seq` for forcing side effects; the canonical pattern is `[w: [side-effect]] [if [= w w] result result]` — forces `w` via equality check, always returns `result`; add this to `doc/08-evaluation.md §Laziness Design` as an explicit idiom note, including why `_` cannot be used (implicit lambda desugaring treats `_` as a lambda parameter, not a discard) (`doc/08-evaluation.md`)
- [ ] **Type checker doesn't propagate scope from intermediate dict expressions:** in a document with multiple sequential expressions (e.g., `[dict1] [dict2] [expr3]`), the type checker fails to carry dict2's bindings into expr3's scope — `expr3` sees T002 "undefined variable" for names defined in dict2, even though the runtime pipeline materialises dict2 and adds its entries to scope; LSP go-to-definition correctly resolves these (confirming they exist), but hover/diagnostics show false errors; fix `typecheck_document` to propagate intermediate dict type environments into subsequent expression scopes the same way `eval_document` propagates runtime bindings (`src/typecheck.rs`)

---

## Capability System

### dir-cap-permissions: Fine-grained read/write/list permissions on DirCap and cap-file

See `doc/whatif/dir-cap-permissions.md` (Accepted 2026-05-11). Extends `--cap-fs` (and `--cap-file`) with an optional `:MODE` suffix using letter bundles and an extended `:[Cap1 Cap2 ...]` list syntax; adds a `DirPerms` bitfield to `Value::DirCap`; enforces permissions in DirCap-consuming builtins; exposes a row-polymorphic `DirCap[Writable ...]` type. No mode on either flag = full access (all capabilities). **Spec chapters:** `doc/whatif/dir-cap-permissions.md`.

**Mode grammar (same for `--cap-fs` and `--cap-file`):**
- No `:mode` suffix → full access (all applicable capabilities)
- Letter sequence: each letter adds its bundle — `r` = `{Readable, Listable, Statable}`, `w` = `{Writable, Appendable, Deletable, Renameable}`, `a` = `{Appendable}`, `s` = `{Statable}`, `l` = `{Listable, Statable}`; letters compose by union (`rw` = r∪w)
- Extended syntax: `:[Cap1 Cap2 ...]` — parse as whitespace-separated capability names, exact set granted, no implied additions; detected by mode starting with `[`
- For `--cap-file`: additional `Binary` flag in extended syntax (`:[Readable Binary]`); letter shorthands `r`/`rb`/`w`/`wb` remain as before (backward compat)

- [ ] Refactor `--cap-fs` argument parsing in `src/main.rs`: split on last `:` via `rsplit_once`; if no `:` present, grant full `DirPerms::full()`; if mode starts with `[`, parse as extended capability list; otherwise parse letter-by-letter accumulating bundles (`r`→Readable+Listable+Statable, `w`→Writable+Appendable+Deletable+Renameable, `a`→Appendable, `s`→Statable, `l`→Listable+Statable); unknown letter = startup error (`src/main.rs`)
- [ ] Extend `--cap-file` argument parsing in `src/main.rs`: same extended syntax — if mode starts with `[`, parse as `[Cap1 Cap2 ...]` list (valid names: `Readable`, `Writable`, `Appendable`, `Binary`); no `:mode` suffix → open file read-write (equivalent to `rw`); retain existing `r`/`rb`/`w`/`wb` letter shorthands for backward compat (`src/main.rs`)
- [ ] Add `DirPerms { readable, statable, listable, writable, appendable, deletable, renameable: bool }` struct to `src/value.rs`; add `perms: DirPerms` field to `Value::DirCap` and `Value::RevocableDirCap`; update all construction sites to use `DirPerms::full()` (`src/value.rs`)
- [ ] Enforce permissions in `builtin_open`: `readable` for `"r"`, `writable` for `"w"`, `appendable` for `"a"`; capability error `"DirCap: open requires <Readable|Writable|Appendable> permission"` on violation (`src/builtins_io.rs`)
- [ ] Enforce `listable` in `builtin_list_dir`; enforce `writable` in `builtin_write`/`builtin_write_atomic`; stubs for future `builtin_delete_file` (needs `deletable`) and `builtin_rename_file` (needs `renameable`) (`src/builtins_io.rs`)
- [ ] Register `%pwd` and `--cap-fs` DirCaps in the type environment with appropriate `DirCap[...]` row types; update builtin type signatures: `list-dir` → `DirCap[Listable ...]`, `open "r"` → `DirCap[Readable ...]`, `open "w"` → `DirCap[Writable ...]` (`src/type_env.rs`)
- [ ] Add `narrow` overload for DirCap: `[narrow cap@DirCap[Flags ...] FlagName...]` produces a new DirCap with the intersection of source permissions and requested flags; runtime error if requested flag is not held; `[narrow cap Subtree "path"]` restricts the directory root to a subdirectory (`src/builtins_io.rs` or new `src/builtins_cap.rs`)
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

## Code Housekeeping

### test-coverage-gaps: Fill critical corpus and unit test coverage gaps

Gaps identified in Cycle #246 analysis. None require design work — these are concrete missing tests.

- [ ] [Major] Type assertion corpus coverage is thin (currently ~4 files in `tests/corpus/eval/typecheck/`) — add ~6 more cases covering: TypeVar constraint propagation through nested dicts, constraint enforcement at multiple call sites for the same generic function, BAS subtyping in TypeAssert position (`@[[all A B]]`, `@[[without A]]`), intersection type narrowing in match arms, and `Type::Error` sentinel cascade prevention (`tests/corpus/eval/typecheck/`)
- [ ] [Major] Laziness proof corpus tests are missing key coverage: add `tests/corpus/eval/lazy/` tests for `$map` on dicts (confirm values remain as PendingCall thunks, not forced), `$filter` selective materialization (predicate forced, non-selected elements not forced), `and`/`or` short-circuit (second arg thunk untouched when result determined by first), lazy `$concat` on seqs (O(1) chain, no element evaluation) (`tests/corpus/eval/lazy/`)
- [ ] [Major] Resource limit corpus tests missing: add end-to-end corpus tests that trigger `MAX_COLLECT_SIZE` (collect into a >1M item dict) and `MAX_STRING_SIZE` (build a >64MB string via `str-repeat`) and verify correct `[E040]`/`[E081]`-coded errors (`tests/corpus/eval/errors/`)
- [ ] [Minor] `split_test_file()` in `tests/test_helpers.rs` has zero unit tests despite being the core test infrastructure parser — add unit tests covering: `=== out` + `=== warn` + `=== error` sections, multiple sections in one file, missing `=== out` section, `#?`/`#!` comment lines, empty file, file with only comments (`tests/test_helpers.rs`)

### stale-todo-cleanup: Remove stale sprint-label TODO comments

These TODO comments reference completed sprints but were never cleaned up. Several reference sprints whose approach was later revised (arena FlatEnv, perf-ast-rc migration). Each item is small — verify, then remove or update.

- [ ] `src/eval_dict.rs:94`: Remove stale `// TODO(ast-rc)` comment block — `perf-ast-rc` sprint is done and the code already uses `Rc::clone(&entry.node.value)`, meaning the migration happened; the comment gives the false impression work is still pending; delete the comment entirely
- [ ] `src/type_unify.rs:681`: Remove stale "when gradual-typing-split is complete, this needs refinement" comment — `gradual-typing-split` sprint is done (DONE.md); verify that the current `Unknown ~ τ as always satisfiable` rule is the correct post-split semantics; if it needs refinement, add a concrete task; otherwise just delete the comment
- [ ] `src/arena.rs:239`: Delete the dead `migrate_for_next_section` function — it is `#[allow(dead_code)]`, contains `unimplemented!()`, and references "arena-eval sprint" which is done (DONE.md); Phase 3 selective migration was never needed since the Rc model was retained throughout
- [ ] `src/resolve.rs:133`: Remove stale `// TODO(arena-phase2)` comment about FlatEnv slot pre-sizing — `arena-phase2` is done (DONE.md) but the FlatEnv/de Bruijn approach was not adopted; replace with a one-line note explaining the current approach (linked environments) is correct
- [ ] `src/eval.rs:602`: Remove stale `// TODO(arena-phase2)` comment about O(1) VarRef slot lookup — same reason as resolve.rs; the linked-environment model is the current design; the `let _ = resolved;` suppressor can remain or be removed depending on whether the field is still useful
- [ ] `src/typecheck.rs:9121` and `src/type_env.rs:1358`: Update stale `// TODO(result-nominal)` comments to say "see `builtin-type-audit` sprint item `try` return type" — result-nominal is done (DONE.md); the remaining work (`try` returning Unknown) is already tracked at TODO.md `builtin-type-audit` line 44
- [ ] `src/parser.rs:3931`: Remove stale `// TODO: Pin patterns ($name) require tracking...` comment — `Pattern::Pin` is fully implemented: parser produces it at line 4082 via the `escaped` field, eval handles it at `eval.rs:1939`, typecheck at `typecheck.rs:1225`, formatter at `formatter.rs:1101`, coverage at `coverage.rs:286`; the comment predates a now-complete implementation
- [x] `doc/12-tooling.md:140`: Fix stale link `doc/whatif/tinct-hosted-formatter.md` → `doc/whatif/completed/tinct-hosted-formatter.md` — done
- [x] `doc/12-tooling.md:142`: Remove broken ref to `doc/whatif/plans/macros-cluster.md` — done; design lives in `doc/whatif/completed/tinct-hosted-formatter.md`

---

## Networking

### net-gaps: QUIC datagrams, SPKI correctness, HTTP/3 concurrent driver

Genuine deferred items from the `http-sessions` and `connector-tls` sprints. Each is a deliberate "implement later" stub.

- [ ] **SPKI X.509 field extraction** (`src/builtins_io.rs:3268`): Replace the current "hash raw DER bytes" workaround with correct SPKI field extraction — parse the certificate DER with `x509-parser` or `rustls-pki-types` to locate the SubjectPublicKeyInfo field, then hash only that field; the current implementation does not match real SPKI pins computed by browsers and tools like `openssl` — this is a correctness bug for TLS pinning users
- [ ] **QUIC unreliable datagram support** (`src/builtins_io.rs:4451-4504`): Implement `quic-open-datagram` builtin — add `Value::QuicDatagramHandle(Rc<quinn::Connection>)` variant; add `send-datagram`/`recv-datagram` overloads dispatching on it via `block_on(conn.send_datagram(...))`/`block_on(conn.read_datagram())`; currently the function returns a user error directing to `quic-open-stream`
- [ ] **HTTP/3 connection driver for concurrent requests** (`src/builtins_io.rs:4695`): The h3 `Connection` driver is currently discarded (`let (_driver, send_request) = ...`), making the session sequential-only. Fix: (1) add `async_rt::spawn<F>(fut: F) -> JoinHandle<F::Output>` that calls `TOKIO_RT.with(|rt| rt.spawn(fut))` — tokio `current_thread` runs spawned tasks during `block_on`; (2) spawn the h3 driver there and store the `JoinHandle` in a new `Http3SessionState { send_request, _driver: JoinHandle<_> }` to keep it alive; (3) change `Value::Http3Session` to wrap `Rc<RefCell<Http3SessionState>>` — no R&D needed, but requires extending the Value type

---

## LSP

### lsp-gaps: Prelude go-to-definition and remaining LSP quality items

**Note:** `lsp-gaps` requires design work before implementation — see Research item below.

- [ ] **Prelude go-to-definition** (`src/lsp/analysis.rs:802`): Parse the embedded prelude source (`include_str!("../../stdlib/prelude.llt")`) once at LSP startup into a `Spanned<File>` AST and cache it in `DocumentStore`; extend `definition_at()` in `src/lsp/analysis.rs` to search the cached prelude AST using the existing `find_key_definition()` recursion after local/include lookup fails; resolve the prelude URI via `find_libdir_path().join("prelude.llt")` + `file_path_to_uri()` for the `Location` response; `llt_span_to_lsp_range` works unchanged since it takes source text separately from spans (`src/lsp/analysis.rs`, `src/lsp/document.rs`)
- [ ] **`textDocument/documentSymbol`:** walk the top-level dict entries of the current document and return them as `SymbolKind::Variable` symbols with their definition spans; add `document_symbols_at` in `src/lsp/analysis.rs`; register `DocumentSymbolRequest::METHOD` in `src/lsp/server.rs`; declare capability in `ServerCapabilities`; enables IDE outline views and breadcrumbs (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`textDocument/formatting`:** call the existing Rust formatter (`src/formatter.rs`) on the full document source and return a single whole-document `TextEdit`; register `DocumentFormattingRequest::METHOD` in `src/lsp/server.rs`; declare `document_formatting_provider` in `ServerCapabilities`; the formatter already produces a round-tripped source string — wrap it in a diff against the original to produce minimal edits, or return a single replace-all edit for simplicity (`src/lsp/server.rs`, `src/formatter.rs`)
- [ ] **`textDocument/references`:** find all spans in the document where a given name is referenced; add `references_at(doc, offset) -> Vec<Location>` in `src/lsp/analysis.rs` — walk the full AST collecting all `Expr::VarRef` nodes whose name matches the symbol under the cursor; register `References::METHOD` in `src/lsp/server.rs`; declare `references_provider` in `ServerCapabilities` (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`textDocument/rename`:** rename a binding and all its references in the document; reuse `references_at` plus the definition span to produce a `WorkspaceEdit` with `TextEdit` entries for every occurrence; validate the new name is a valid tinct identifier before returning; register `Rename::METHOD` in `src/lsp/server.rs`; declare `rename_provider` in `ServerCapabilities` (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`textDocument/inlayHints`:** return inferred types inline next to unannotated bindings in the visible range; add `inlay_hints_in_range(doc, range) -> Vec<InlayHint>` in `src/lsp/analysis.rs` — for each top-level dict entry whose value is not annotated, look up its inferred `TypeScheme` from the type map and emit a hint with the display string (e.g., `: Int`, `: Fn@Bool [a a]`) positioned after the binding name; register `InlayHintRequest::METHOD` in `src/lsp/server.rs`; declare `inlay_hint_provider` in `ServerCapabilities`; this is the highest-information-density feature for a type-inferred language (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`textDocument/signatureHelp`:** when the cursor is inside a function call `[f ...]`, look up `f`'s `TypeScheme`, extract parameter names and types, and return a `SignatureInformation` showing the full `Fn@Return [param1@Type ...]` signature with the active parameter highlighted based on cursor position; register `SignatureHelpRequest::METHOD` in `src/lsp/server.rs`; declare `signature_help_provider` in `ServerCapabilities` (`src/lsp/server.rs`, `src/lsp/analysis.rs`)
- [ ] **`workspace/symbol`:** search all top-level bindings across all open and recently-loaded documents matching a query string; return as `WorkspaceSymbol` entries with their file URIs and definition ranges; register `WorkspaceSymbolRequest::METHOD` in `src/lsp/server.rs`; declare `workspace_symbol_provider` in `ServerCapabilities` (`src/lsp/server.rs`, `src/lsp/document.rs`)
- [ ] [Major] Verify LSP `document.rs` `update_document` calls `desugar_file()` BEFORE `typecheck_file()` — all other entry points follow `expand_macros → desugar → resolve → typecheck → eval`; if LSP reorders or skips desugar, the type checker sees `VarRef("_")` instead of desugared `Fn` nodes producing spurious "undefined variable _" errors; confirm and add a PIPELINE INVARIANT comment (`src/lsp/document.rs`)

---

## Evaluator and Macros

### eval-gaps: Unquote nesting, error span threading

Two correctness/quality gaps in the evaluator noted in source comments.

- [ ] **Unquote in nested positions** (`src/eval.rs:1343`): The `eval_quote` fallback arm (`_ =>`) calls `ast_to_dict_expr` which does not recognize `Expr::Unquote`/`Expr::UnquoteSplice` in nested positions; add a recursive `eval_quote_expr` pre-pass in `src/eval.rs` that walks the full `Expr` tree — when it encounters `Expr::Unquote(inner)`, evaluate `inner` and substitute the result as a serialized AST value node; when it encounters `Expr::UnquoteSplice(inner)` in a list position, splice the evaluated sequence; all other nodes recurse unchanged; replace the `_ =>` arm with a call to `eval_quote_expr` then `ast_to_dict_expr`; `ast_to_dict_expr` is unchanged (`src/eval.rs`); add corpus tests for nested `[unquote ...]` inside call args, dict values, and seq literals (`tests/corpus/eval/`)
- [ ] **`mat_span` threading through DotAccessForceData** (`src/eval_materialize.rs:1344`, `src/eval_materialize.rs:1379`): When `.field` access in an access chain triggers materialization, the `mat_span` used is the access expression span rather than the outer materialization context's span — this loses the outermost call-site span in error messages for chained access like `a.b.c`; fix by threading `outer_mat_span: Option<Span>` through `DotAccessForceData` and using it in `Action::Materialize`; corresponding test is at `src/eval.rs:5559` (currently asserts the wrong span as a known limitation — update when fixed)

---

## CLI

### cli-gaps: --libdir-path override and other deferred CLI features

- [ ] **`--libdir-path PATH` flag** (`src/main.rs:1106`): Add CLI flag to override the standard library directory — the comment at line 1106 was deferred from `io-phase2` (which is done); useful for custom installations or alternative stdlib testing; wire through `main.rs` arg parsing, override the auto-detected `%libdir` in the root env; add `--help` text and a test

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
