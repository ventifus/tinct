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

### macro-expansion-boundary: Fix macro expander arena isolation (prerequisite for hkt-do-macro-*)

**Root cause (panel review 2026-05-13):** `expand_macros` creates a fresh `ThunkArena` (Arena-B), but `create_stdlib_env()` creates its own separate arena (Arena-A). Macro transformer closures from the stdlib contain ThunkIds pointing into Arena-A. When `deep_materialize` runs using Arena-B's context, `ctx.get_thunk(id)` panics: the ID is an out-of-bounds index into the wrong Vec.

**Architectural decision — Option C (phase separation):** Macro expansion boundaries are *data boundaries*. Inputs and outputs must be fully materialized — no arena-relative ThunkId handles may cross them. This is consistent with the Racket syntax-object model and is forward-compatible with Phase 3 per-section arena lifetimes (Option D — sharing arenas — defeats per-section reclamation).

The output side is already correct (`deep_materialize` at `expand.rs:886`). Only the input side needs fixing.

- [ ] In `expand_macro_call` (`src/expand.rs`): after `ast_to_dict_expr` produces the quoted-args AST dict thunk at `expand.rs:801`, call `deep_materialize` on it before passing to the transformer; since `ast_to_dict_expr` creates only `Thunk::new_materialized` entries, every force is a cache hit — no lazy computation is triggered, cost is O(AST node count) (`src/expand.rs`)
- [ ] Add `doc` comment to `expand_macro_call` stating the invariant: "The macro expansion boundary is a data boundary. Both the input AST dict and the output AST dict are fully materialized before crossing. No arena-relative ThunkId handles may flow from the stdlib arena into the expansion arena or vice versa." (`src/expand.rs`)
- [ ] Add `debug_assert!` after both the input deep-materialize and the existing output `deep_materialize` confirming all thunks in the value are `Materialized`; this makes the invariant machine-checkable in debug builds (`src/expand.rs`)
- [ ] Tests: write a macro transformer that reads from its own closure (stdlib env) and returns a dict that includes those closure values; verify no panic (`tests/corpus/eval/macros/`)

### hkt-do-macro-explicit: Implement [do] macro — explicit form

See `doc/whatif/completed/hkt-monads.md` §`[do]` Inference. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §[do] Inference`.

The explicit `[do monad steps...]` form has **no HKT dependency** — it desugars to `monad.bind` field access on a plain dict.

- [ ] Implement explicit `[do monad steps...]` desugaring in `stdlib/macros.llt` via the existing `STDLIB_MACROS` registration path: classify each step as binding (`[name: expr]`) or non-binding (bare expression) by inspecting the AST dict shape; bindings → `[monad.bind expr [fn [name] <rest>]]`; non-bindings → `[monad.bind expr [fn [_] <rest>]]`; last step is the return value with no wrapping; `[do monad]` with no steps → `[monad.pure []]`; `[do]` with zero args → error (`stdlib/macros.llt`, `src/expand.rs`)
- [ ] Tests: `[do result [r: [fetch ...]]]` three-step success, `[Err "fail"]` propagation (short-circuit), `[do]` with any `bind:`-carrying dict (not just Result), `[do monad]` no-steps → `[monad.pure []]`, zero-args error (`tests/corpus/eval/`)

**Depends on:** `macro-expansion-boundary`

### hkt-do-macro-inferred: [do] macro — inferred monad form

The inferred `[do steps...]` form (monad argument omitted, inferred from return type or first binding). Requires `hkt-kind-inference` to provide `App` type inference and `kind_env`-based Monad class lookup.

- [ ] Add `expected_return: Option<Type>` field to `InferState` in `src/types.rs:1590` (alongside `kind_env: HashMap<String, Kind>`); set by `infer_fn` before descending into fn body when explicit return annotation is present; avoids cascading `infer_expr` signature changes (`src/types.rs`)
- [ ] In `src/expand.rs`: when `[do]` has no explicit monad arg, emit `[do %do-infer steps...]` sentinel — `Expr::VarRef("%do-infer")` as the monad argument; runtime never sees this sentinel — it is resolved and substituted by the type checker before eval (`src/expand.rs`)
- [ ] In `src/typecheck.rs` `infer_expr` for `[do]`: when monad is `VarRef("%do-infer")`, resolve monad via: (1) `state.expected_return` unified against `App(m, _)` for a registered Monad class; (2) first binding RHS type `App(m, a)` for a known Monad; (3) emit "cannot infer monad — add explicit monad arg or annotate return type" on failure; substitute resolved monad name into the desugared `[monad.bind ...]` chain before evaluation (`src/typecheck.rs`)
- [ ] Tests: inferred `[do]` from `fn@Result` return annotation, inferred from first binding type, unresolvable monad error, `[do]` inside HKT-generic function (`tests/corpus/eval/`)

**Depends on:** `hkt-do-macro-explicit`, `macro-expansion-boundary`


### infer-dict-class-preregistration: Pass 0c — pre-register class/instance declarations before SCC processing

**Panel finding (2026-05-13):** `satisfies_constraint` at `src/type_unify.rs:14–50` uses hardcoded match arms. Removing them requires instances to be registered in `state.instance_env` before constrained functions are type-checked. But `infer_dict`'s SCC loop processes class/instance declarations after functions that use constraints (they're later in the file, independent SCCs emit in array index order). Identical to how type aliases are pre-registered in Pass 2 — class/instance declarations need their own pass.

- [ ] Add Pass 0c to `infer_dict` in `src/typecheck_dict.rs`, between Pass 2 (type alias pre-registration, line ~262) and the SCC loop (line ~275): iterate all entries, call `infer_expr` on any `Expr::ClassDecl` or `Expr::InstanceDecl` to register them into `state.class_env`/`state.instance_env` before bodies are type-checked; ~10 lines modeled on Pass 2 (`src/typecheck_dict.rs`)
- [ ] Add doc comment: "Pass 0c: pre-register class/instance declarations so all classes and instances are visible during body type-checking, regardless of declaration order in the file (Wadler & Blott 1989 — class/instance declarations are globally visible)" (`src/typecheck_dict.rs`)
- [ ] Tests: class declaration appearing AFTER a function that uses its constraint in the same dict; confirm no error (`tests/corpus/eval/typecheck/`)

### hkt-mappable-appendable: Rewrite Mappable and Appendable from hardcoded to class-based

See `doc/whatif/completed/hkt-monads.md` §The Typeclass Hierarchy §Mappable, §Appendable. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §The Typeclass Hierarchy`.

`hkt-kind-inference` delivers: (1) class param annotations parsed and wired to `kind_env` — `[Mappable: [class [f@Operator] ...]]` now works; (2) `@[f a]` in annotation position produces `Type::App(f, a)` — instance method type signatures like `[fn@[f b] [[f a]]]` are typeable. This sprint builds on those two foundations.

**Phase 1 — resolve_instance freshening (enables parameterized instance heads):**
- [x] Fix `resolve_instance` in `src/type_env.rs`: freshen all free type vars in `inst.instance_type` via `instantiate_at_level` before unification (`src/type_env.rs`)

**Phase 2 — Mappable migration:**
- [x] Write `Mappable` class + `MappableSeq`/`MappableRecord` instances in `stdlib/prelude.llt` (`stdlib/prelude.llt`)
- [ ] Remove hardcoded `Mappable` from `satisfies_constraint` match in `src/type_unify.rs` — **NOT YET DONE** (panel audit 2026-05-13: hardcoded arm still present at `src/type_unify.rs:43-47`); gate on `infer-dict-class-preregistration` landing first (`src/type_unify.rs`)
- [ ] Remove `Mappable` placeholder pre-registration from `InferState::new()` in `src/types.rs` — gate on hardcoded arm removal above (`src/types.rs`)
- [ ] Update `$map`/`$filter` type signatures in `src/type_env.rs` to use `Mappable f` constraint instead of hardcoded dual-dispatch (`src/type_env.rs`)

**Phase 3 — Appendable migration:**
- [x] Write `Appendable` class + instances in `stdlib/prelude.llt` (`stdlib/prelude.llt`)
- [ ] Remove hardcoded `Appendable` from `satisfies_constraint` match in `src/type_unify.rs` — **NOT YET DONE** (same audit; hardcoded arm still present); gate on `infer-dict-class-preregistration` (`src/type_unify.rs`)
- [ ] Remove `Appendable` placeholder pre-registration from `InferState::new()` (`src/types.rs`)
- [ ] Update `$concat`/`$conj` type sigs in `src/type_env.rs` to use `Appendable a` (`src/type_env.rs`)

**Phase 4 — Equatable, Comparable, Showable migration (no chicken-and-egg — pre-registrations in `InferState::new()` at `src/types.rs:1619-1649` handle forward refs; `infer-dict-class-preregistration` handles ordering within `infer_dict`):**
- [ ] Write `Equatable` class + instances for `Int`, `Str`, `Bool`, `Float`; remove from `satisfies_constraint` (`src/type_unify.rs:16-41`) and `InferState::new()` (`src/types.rs:1620-1635`) (`stdlib/prelude.llt`, `src/type_unify.rs`, `src/types.rs`)
- [ ] Write `Comparable` class (extends Equatable) + instances for `Int`, `Str`, `Float`; remove from `satisfies_constraint` and `InferState::new()` (`stdlib/prelude.llt`, `src/type_unify.rs`, `src/types.rs`)
- [ ] Write `Showable` class + instances for `Int`, `Str`, `Bool`, `Float`, `Null`; remove from `satisfies_constraint` and `InferState::new()`; `Numeric` stays hardcoded (`stdlib/prelude.llt`, `src/type_unify.rs`, `src/types.rs`)
- [ ] Verify prelude annotations from `builtin-type-audit` batch B still type-check after migrations (`stdlib/prelude.llt`)
- [ ] Tests: user-defined `Equatable`/`Comparable`/`Showable` instances; `=` on non-Equatable type errors; `satisfies_constraint` no longer special-cases any migrated class (`tests/corpus/eval/typecheck/`)

**Depends on:** `infer-dict-class-preregistration`


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

## Standard Library Boundary

### stdlib-boundary: stdlib Rust surface area reduction

Audit findings (2026-05-13): most I/O builtins genuinely require Rust (28 irreducible syscall/opaque-type primitives). These specific ones do not. Also adds missing Rust primitives that unlock tinct migrations, and verifies all stdlib modules use `%rust` groups cleanly after `primitive-privacy` Phase 3 lands.

- [x] Move `spki-pin` to tinct in `stdlib/net.llt`: pure dict construction, no syscalls, no Rust crates — verified in `stdlib/net.llt:94` (commit 2ea7563)
- [x] Add `raw-create : DirCap → Str → WriteHandle` Rust primitive: opens a file for writing (create/truncate) returning a `WriteHandle` — implemented in `src/builtins_io.rs:1804` (2026-05-11)
- [x] Once `raw-create` lands, rewrite `copy` in tinct: `[fn [cap src dst] [close [write-handle [raw-create cap dst] [slurp [open cap src Readable Text]]]]]` — implemented in `stdlib/io.llt:71`, removed Rust builtin (2026-05-11)
- [x] Change `cap-data` to return `Null` (empty dict `[]`) when the capability name is not present instead of erroring — verified in `src/builtins_io.rs:1534` (commit 2ea7563)
- [x] Once `cap-data` returns null on miss, rewrite `has-cap?` in tinct as `[fn [h cap] [not [null? [cap-data h cap]]]]` — verified in `stdlib/io.llt:64` (commit 2ea7563)
- [x] Investigate and remove the vestigial Rust `http-get` builtin (the `HttpConn` form using `Value::HttpConn`/reqwest client directly) — verified absent from `src/builtins_io.rs` (commit 2ea7563)
- [x] Add `str-index-of : Str → Str → Int` Rust builtin: native O(n) substring search returning start byte-index or -1 on miss; wraps `str::find`; replaces the O(n²) `str-find-impl` in prelude with an O(n) call (`src/builtins_string.rs`, `src/type_env.rs`, `stdlib/prelude.llt`)
- [x] Once `str-index-of` lands, rewrite `str-contains?`, `starts-with?` (string form), `ends-with?` (string form) in `stdlib/strings.llt` as tinct wrappers — implemented in `stdlib/strings.llt:92-107`, removed 3 Rust builtins, builtin count 184→181 (2026-05-11)
- [x] Add `str-map-chars : (Str → Str) → Str → Str` Rust builtin: map a tinct function over Unicode codepoints, returning a new string; unlocks `upper`, `lower`, and character-level transforms as tinct stdlib functions (`src/builtins_string.rs`, `src/type_env.rs`)
- [x] Once `str-map-chars` lands, rewrite `upper` and `lower` in `stdlib/strings.llt` as tinct functions using `str-map-chars` + `char-code`/`chr` arithmetic for ASCII fast path; remove Rust builtins (`stdlib/strings.llt`, `src/builtins_string.rs`)
- [x] Add `trim-start : Str → Str` and `trim-end : Str → Str` Rust builtins: strip leading/trailing whitespace from one end only (`src/builtins_string.rs`, `src/type_env.rs`)
- [x] Add `regex-match? : Str → Str → Bool` Rust builtin: test if a regex pattern matches anywhere in a string using the `regex` crate; unlocks the `pattern` constraint in `validate` and other regex-dependent stdlib functions as tinct code (`src/builtins_string.rs`, `src/type_env.rs`)
- [ ] Once `regex-match?` lands, rewrite `validate`'s `pattern` constraint check in tinct; identify which other parts of `validate` can move to tinct vs what must remain Rust (`stdlib/prelude.llt` or `src/builtins_meta.rs`) — **DEFERRED**: `validate` is a 350-line recursive Rust function (`validate_value`); moving just the pattern check requires calling back into the tinct evaluator from within Rust recursion; the only gain would be calling the `regex-match?` Rust function instead of `regex::Regex::new()` directly, which is still Rust; a full rewrite in tinct (the only real migration) requires recursive dict schemas and is a separate sprint
- [ ] Make `builtin-sort` accept an optional comparator argument: `builtin-sort : ((a → a → Bool)? → Dict → Dict)`; when provided, use comparator instead of natural type ordering; this allows `sort` and `sort-by` in prelude to both reduce to one Rust primitive (`src/builtins_seq_prim.rs`, `src/type_env.rs`)
- [x] Verify `stdlib/prelude.llt` imports exactly: `rust::core`, `rust::string`, `rust::collection`, `rust::json`, `rust::meta`; no bare Rust primitive references outside these groups; no `builtin-*` names remain — **VERIFIED 2026-05-11**: prelude opens with 8 `[include %rust "..."]` groups (core, collection, string, bytes, math, meta, json, io); the listed primitives (`builtin-*`) are now only in scope via `inject_prelude_aliases()` called at stdlib_env creation after prelude loads — they are NOT in prelude's dict and are not directly called by prelude code (prelude uses its own local `builtin-*` names obtained from the `%rust` includes) (`stdlib/prelude.llt`)
- [x] Verify `stdlib/io.llt`, `stdlib/net.llt`, `stdlib/math.llt`, `stdlib/datetime.llt`, `stdlib/encoding.llt` each open with exactly one `[include %rust "..."]` and build entirely on prelude exports + their imported group — **VERIFIED 2026-05-11**: io.llt→`[include %rust "io"]`; net.llt→`[include %rust "net"]`+`[include %rust "io"]`+`[include %rust "string"]` (3 groups needed since net uses write-handle from io and split from string); math.llt→`[include %rust "math"]`; datetime.llt→`[include %rust "datetime"]`; encoding.llt→`[include %rust "core"]`+`[include %rust "string"]`+`[include %rust "math"]` (bxor from math); strings.llt→`[include %rust "core"]`+`[include %rust "string"]`; all build on prelude exports for higher-level functions (`stdlib/*.llt`)
- [ ] Verify that intentionally unexported primitives (`eval-ast`, `gensym`, `llt-repr`, `tag-of`, `variant`, `decimal`, `big-int`, `proxy`) are not accessible from user code — write corpus tests confirming `undefined variable` (`tests/corpus/eval/`) — **DESIGN CONFLICT**: `eval-ast`, `gensym`, `llt-repr`, `tag-of`, `variant` are documented as user-facing builtins in `doc/11a-builtins.md`; `gensym` is actively used by macro authors for hygiene (`tests/corpus/eval/macros/hygiene_no_capture.llt-eval`); the `%rust` registry is accessible from user scope via the bootstrap_env parent chain (code comment says "Full isolation (hiding %rust from user scope) is future work"); items remain accessible via the backward-compat injection in `create_stdlib_env_inner`; **actual future work**: remove the backward-compat injection and decide which of these to keep as public API vs truly internal
- [x] Update `doc/11-stdlib.md §Rust-Native vs Tinct-Implemented Boundary` to document the `%rust` virtual module system and which modules each stdlib file imports (`doc/11-stdlib.md`)

---

## Miscellaneous

### misc-gaps: Miscellaneous small gaps across the codebase

Accepted 2026-05-11. See `doc/whatif/multi-line-strings.md` (triple-quote lexer). Genuine deferred items from the `http-sessions` and `connector-tls` sprints. Two correctness/quality gaps in the evaluator noted in source comments. Extends `--cap-fs` and `--cap-file` with fine-grained read/write/list permissions.

- [ ] Add `TripleQuotedString(String)` and `TripleInterpolatedString(Vec<InterpolatedPart>)` token types to `src/lexer.rs`: detect `"""` at the start of a string context, consume content until the closing `"""`, emit accordingly; then in `src/parser.rs` desugar `TripleQuotedString(s)` → `[unindent s]` and `TripleInterpolatedString(parts)` → `[unindent i"..."]` directly in the parser (not as a stdlib macro, since macros cannot intercept token patterns) (`src/lexer.rs`, `src/parser.rs`)
- [ ] Tests: `quic-open-datagram` + `send-datagram` + `recv-datagram` round-trip corpus test; `http3-session` concurrent request (two sequential requests on one session succeed); QUIC datagram type error on wrong handle type (`tests/corpus/eval/`)
- [ ] **Unquote in nested positions** (`src/eval.rs:1343`): The `eval_quote` fallback arm (`_ =>`) calls `ast_to_dict_expr` which does not recognize `Expr::Unquote`/`Expr::UnquoteSplice` in nested positions; add a recursive `eval_quote_expr` pre-pass in `src/eval.rs` that walks the full `Expr` tree — when it encounters `Expr::Unquote(inner)`, evaluate `inner` and substitute the result as a serialized AST value node; when it encounters `Expr::UnquoteSplice(inner)` in a list position, splice the evaluated sequence; all other nodes recurse unchanged; replace the `_ =>` arm with a call to `eval_quote_expr` then `ast_to_dict_expr`; `ast_to_dict_expr` is unchanged (`src/eval.rs`); add corpus tests for nested `[unquote ...]` inside call args, dict values, and seq literals (`tests/corpus/eval/`)
- [x] Extend `--cap-file` argument parsing in `src/main.rs`: same extended syntax — if mode starts with `[`, parse as `[Cap1 Cap2 ...]` list (valid names: `Readable`, `Writable`, `Appendable`, `Binary`); no `:mode` suffix → open file read-write (equivalent to `rw`); retain existing `r`/`rb`/`w`/`wb` letter shorthands for backward compat (`src/main.rs`)
- [x] Implement `open` write and append paths in `builtin_open`: the `Writable` and `Appendable` flag branches currently return "not yet implemented" (`src/builtins_io.rs:197,371`); implement using `dir.open_with(path, OpenOptions::new().write(true).create(true).truncate(true))` for Writable and `.append(true)` for Appendable; wrap result in `Value::WriteHandle` with appropriate caps (`src/builtins_io.rs`)
- [ ] Register capability flag nominal unit types in `TypeEnv`: `Readable`, `Writable`, `Listable`, `Statable`, `Appendable`, `Deletable`, `Renameable` — each as a singleton `Type::NominalTag` (no payload); register `%pwd` and `--cap-fs` DirCaps using BAS intersection types; update builtin type signatures using `Type::Intersection([DirCap, Flag])`: `list-dir` → `Intersection([DirCap, Listable])`, `open "r"` → `Intersection([DirCap, Readable])`, `open "w"` → `Intersection([DirCap, Writable])`; the subtyping `Intersection([DirCap, Readable, Writable]) <: Intersection([DirCap, Writable])` holds automatically under BAS intersection elimination (`src/type_env.rs`, `src/types.rs`)
- [x] Add `narrow` overload for DirCap: `[narrow cap@[[all DirCap Flag1 ...]] FlagName...]` produces a new DirCap with the intersection of source permissions and requested flags; the return type is `Intersection([DirCap, requested-flags])` — a BAS intersection narrower than the input; runtime error if a requested flag is not held in the source `DirPerms`; `[narrow cap Subtree "path"]` restricts the directory root to a subdirectory and returns the same intersection type with an updated root path (`src/builtins_io.rs` or new `src/builtins_cap.rs`)
- [x] Tests: `--cap-fs root=.:r` → `list-dir` succeeds, `open "w"` fails; `--cap-fs data='./d:[Readable Statable]'` → read succeeds, `list-dir` fails; `--cap-file cfg=Cargo.toml` (no mode) → read-write handle; extended syntax `--cap-file cfg='Cargo.toml:[Readable]'` → read-only handle; `narrow` reduces permissions; `narrow` to non-held flag errors (`tests/corpus/eval/`, `tests/corpus/cli/`)


