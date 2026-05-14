# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Substitution Performance

### subst-path-compression: Path compression in Substitution::apply_inner

Targeted fix for the O(N²) substitution merge loop in `infer_dict` (`src/typecheck_dict.rs:383-409`). The loop calls `subst.apply(&v)` for each entry, and `apply_inner` follows TypeVar chains of depth ≥4. Path compression compresses these chains on first traversal, making subsequent lookups O(1) amortized.

See `doc/whatif/union-find-substitution.md §Prerequisites` for the documented blocker.

- [ ] Add path compression to `Substitution::apply_inner()` in `src/types.rs`: after resolving a TypeVar chain `t0 → t1 → ... → concrete`, update `type_map` to map `t0`, `t1`, ... directly to the resolved `concrete` type (skipping intermediate nodes). This collapses chains on first traversal — amortized O(1) lookups thereafter. ~15 lines; no struct changes needed. (`src/types.rs`)
- [ ] Tests: verify `[fn [x] x]` still infers correctly; verify `just test-lib` passes with 64MB stack; spot-check that prelude type-checking completes in < 5s (`tests/`, `stdlib/prelude.llt`)

---

## Research (requires /rnd before implementing)

- [x] Research runtime reflection — see `doc/whatif/runtime-reflection.md`. Accepted 2026-05-14. Design: `FnAnnotation` on `Value::Function`; `ast-of` Rust primitive in `%rust "meta"`; `describe`/`sig-from-ast`/`annotation-to-str`/`annotation-of`/`source-of` in prelude; enables REPL `:describe`, LSP doc hover, docgen, and metaprogramming.

- [x] Research constraint annotations — see `doc/whatif/constraint-annotations.md`. Decision: `fn@[...]` becomes a named-key metadata dict with `return:`, `constraint:`, and `doc:` keys; `constraint: [a: Comparable]` uses binding syntax (lowercase TypeVar key, uppercase class value); `fn@Type` shorthand permanent.

- [x] Research union annotations with named TypeVars — verified: `ann_mapping` propagates through all positional union entries in `resolve_annotation` → `resolve_type_expr` → `resolve_type_name`; `a` in `fn@[a Null]` shares the same TypeVar as `body@a`. **This is a sprint, not research.** Follow-up tasks added to `prelude-type-annotations` below. Prerequisite: `constraint-annotations` sprint (fixes `fn@[...]` positional-union path).

- [x] Research row-access types for `get`/`get-in` — merged into `doc/whatif/completed/hkt-monads.md §Field Access Typing`. Design: `HasField` qualified-type constraint (G-J-for-BAS); `Kind::Label`; `[HAS-FIELD-REC/UNION/INTER/TOP]` BAS rules; `[GET]`/`[GET-IN]` type rules; label-polymorphic `get`/`get-in`; Castagna (2023) formally proves union distribution. Implementation lands in `hkt-foundation` + `hkt-mappable-appendable`.

- [x] Research LSP prelude go-to-definition — `Span` carries no file path but `find_definition` already returns `(Uri, Span)` as separate values; `llt_span_to_lsp_range` takes source text separately, so path-less spans work fine. Approach: parse prelude once at LSP startup using the embedded `include_str!()` source; cache the `Spanned<File>` AST; extend `definition_at()` to search it after local/include miss; resolve URI via `find_libdir_path().join("prelude.llt")` + `file_path_to_uri()`. **This is a sprint.** Tasks added to `lsp-gaps`.

- [x] Research inference completeness — see `doc/whatif/inference-completeness.md`. Design: SCC-based binding group analysis (Tarjan + topological sort within DICT-GEN) eliminates letrec monomorphism and nested dict polymorphism simultaneously; no value restriction (pure language); polymorphic recursion rejected with clear error; variadic params typed as `Seq(T)` with call-site unification; typeclass-based heterogeneous variadics (FormatResult pattern) for printf-style use cases. Three related gaps in tinct's HM inference engine, all addressable together: (1) **letrec monomorphism** — all entries in a letrec group are monomorphic with respect to each other; forward references see a fresh TypeVar rather than a generalized scheme; can DICT-GEN be extended to generalize entries independently? (Mycroft 1984, Kiselyov 2013 levels); (2) **nested dict let-polymorphism** — only top-level dict entries receive DICT-GEN Pass 4 generalization; inner entries remain at the outer level; can inner entries be generalized independently while respecting letrec scoping? (3) **typed variadic parameters** — `...args` is typed `Unknown` because the runtime collects remaining args into an Int-keyed Dict; can variadics collect into a typed `Seq[T]` instead, requiring a runtime representation change?

- [x] Research advanced typeclass extensions — see `doc/whatif/advanced-typeclasses.md`. Design: 3-parameter `Add a b c | (a,b)→c` MPTC with functional dependencies for precise mixed-mode arithmetic; row-level constraint propagation via BAS intersection distribution ([CONSTRAIN-FIELD/INTER/UNION]); runtime ClassEnv dispatch extending primitive operator builtins to user-defined instances; all three extend the same Constraint infrastructure and share the ClassEnv registry. Three tightly-interlinked extensions to the typeclass system beyond the HKT baseline, all extending the same `Constraint` infrastructure: (1) **multi-parameter type classes for Numeric** — `[+ Int Float] → Float` requires MPTCs; `Numeric` stays hardcoded because single-parameter classes cannot express coercion typing (Jones 1995 functional dependencies, Peyton Jones et al. 1997 type improvement); (2) **row-level constraints** — `Equatable [name: a ...]` (all fields satisfy a constraint) requires row-level constraint propagation under BAS; what does `Homogeneous` look like over BAS intersections? (Gaster & Jones 1996, PureScript); (3) **runtime typeclass dispatch** — user-defined instances cannot intercept primitive operators (`=`, `<`, `str`) because builtins dispatch via Rust type inspection, not via instance dictionaries; what would dictionary translation (Wadler & Blott 1989, Jones 1995) look like for tinct?

---

## Inference Completeness

Accepted 2026-05-14. See `doc/whatif/inference-completeness.md` and `doc/06-type-inference.md §Multi-Parameter Type Classes`, `§Nested Dict Polymorphism`, `§Variadic Call`. SCC-based DICT-GEN is already implemented (`src/typecheck_dict.rs`). Two remaining sub-features: variadic `Seq(T)` and nested dict polymorphism via `TypeScheme.inner_schemes`.

- [x] Research inference completeness — see `doc/whatif/inference-completeness.md`. Accepted 2026-05-14.

### inference-completeness-variadic: Type variadic params as Seq(T) with call-site unification

See `doc/06-type-inference.md §[FN-VARIADIC] / [CALL-VARIADIC]`. **Spec chapters:** `doc/06-type-inference.md §Inference Judgments`.

- [ ] `infer_fn` in `src/typecheck.rs:3357–3362`: change variadic param typing from `Type::Unknown` to `Type::Seq(state.fresh_type_var(span))` — a fresh TypeVar β per function; update comment from "Any" to "Seq(β)" (`src/typecheck.rs`)
- [ ] `check_call` / `check_call_with_scheme` in `src/typecheck.rs`: add [CALL-VARIADIC] path — when function type has a `Seq(β)` variadic param, widen each variadic argument type (IntLiteral→Int, FloatLiteral→Float, StrLiteral→Str) then unify against β; error span on the specific failing argument (`src/typecheck.rs`)
- [ ] `eval_call.rs:330–344` (BIND-VARIADIC): change variadic arg collection from `Value::Dict` with integer keys to `Value::Seq` — a `Vec`-backed seq value; update comment (`src/eval_call.rs`)
- [ ] Audit `stdlib/prelude.llt` for functions using variadic params (`->`, `str`, etc.) that access the collected args by integer key (`args.0`, etc.) — migrate to Seq operations (`each`, `map`, `reduce`, index via `get`) (`stdlib/prelude.llt`)
- [ ] Update corpus test at `tests/corpus/eval/` that asserts `rest = Dict({0: 2, 1: 3})` for variadic collection — update to Seq assertion
- [ ] Tests: `[sum 1 2 3]` infers `Numeric α => α`; `[sum 1 "two" 3]` type errors at arg 2 with span on `"two"`; `[fn [...xs] xs]` infers `Fn@Seq(α) []`; zero-variadic-args case (`tests/corpus/eval/typecheck/`, `tests/corpus/eval/builtins/`)

### inference-completeness-nested-dict: Polymorphic dot-access via TypeScheme.inner_schemes

See `doc/06-type-inference.md §Nested Dict Polymorphism`. **Spec chapters:** `doc/06-type-inference.md §Nested Dict Polymorphism`.

- [ ] Add `pub inner_schemes: Option<HashMap<String, TypeScheme>>` field to `TypeScheme` in `src/types.rs:1509`; default `None` in `TypeScheme::mono` and all existing `TypeScheme` construction sites (`src/types.rs`)
- [ ] After Pass 4 generalization in `src/typecheck_dict.rs`: when `infer_dict`'s result scheme map is non-empty, store it in the binding's `TypeScheme.inner_schemes`; determine the binding site (the upstream caller that inserts into `TypeEnv`) and set `inner_schemes: Some(field_schemes)` there (`src/typecheck_dict.rs`, `src/typecheck.rs`)
- [ ] `check_dot_access` in `src/typecheck.rs:2564`: add `VarRef` fast-path before calling `infer_expr` — if target is `Expr::VarRef(name)` and `env.get(name)` yields a scheme with `inner_schemes: Some(inner)`, look up the field name in `inner` and call `instantiate_scheme(field_scheme, state.level, state)`; otherwise fall through to existing path (`src/typecheck.rs`)
- [ ] Ensure `inner_schemes` propagates correctly through `TypeEnv` chain (visible-literal boundary: only dict literals get `Some`; function parameters and cross-file opaque types get `None`) (`src/type_env.rs`)
- [ ] Tests: `helpers.id` where `helpers: [id: [fn [x] x]]` — call at two different types in same file both succeed; `helpers.id` passed as function arg is opaque (monomorphic); cross-file include dict access is opaque; conditional-expression dict target falls through to existing path (`tests/corpus/eval/typecheck/`)

---

## Advanced Typeclass Extensions

Accepted 2026-05-14. See `doc/whatif/advanced-typeclasses.md` and `doc/06-type-inference.md §Constraint Propagation over BAS Types`, `§Multi-Parameter Type Classes`. `[CONSTRAIN-UNKNOWN]` fix already applied (`src/type_unify.rs:25`). Row-level propagation has no HKT dependency. MPTC work requires `hkt-mappable-appendable`.

- [x] Research advanced typeclass extensions — see `doc/whatif/advanced-typeclasses.md`. Accepted 2026-05-14.

### typeclass-constraint-propagation: CONSTRAIN-FIELD/INTER/UNION/TOP/NEVER propagation rules

See `doc/06-type-inference.md §Constraint Propagation over BAS Types`. **Spec chapters:** `doc/06-type-inference.md §Constraint Propagation over BAS Types`. No HKT dependency — can sprint now.

Note: `[CONSTRAIN-UNKNOWN]` (`Type::Unknown => return true`) is already applied at `src/type_unify.rs:25`.

- [ ] Add `[CONSTRAIN-FIELD]` arm to `satisfies_constraint` in `src/type_unify.rs`: for `Type::Record(row)`, iterate `row.fields.values()` and return `fields.values().all(|ty| satisfies_constraint(ty, class_name))`; applies only when class_name is one of the built-in classes (not a catch-all for user-defined classes) (`src/type_unify.rs`)
- [ ] Add `[CONSTRAIN-INTER]` arm: for `Type::Intersection(members)`, return `members.iter().all(|m| satisfies_constraint(m, class_name))` (`src/type_unify.rs`)
- [ ] Add `[CONSTRAIN-UNION]` arm: for `Type::Union(members)`, return `members.iter().all(|m| satisfies_constraint(m, class_name))` — ALL members, not any (`src/type_unify.rs`)
- [ ] Add explicit `[CONSTRAIN-NEVER]` arm: for `Type::Never`, return `true` (vacuously — uninhabited) (`src/type_unify.rs`)
- [ ] Add explicit `[CONSTRAIN-TOP]` arm: for `Type::Top`, return `class_name == "Showable"` — explicitly `false` for all non-Showable classes, matching the formal rule and replacing the accidental fall-through (`src/type_unify.rs`)
- [ ] Tests: `Equatable({name: Str, age: Int})` propagates to `Equatable(Str) ∧ Equatable(Int)` → satisfied; `Equatable({f: Fn@Int []})` fails at Fn field; `Equatable(Int | Str)` satisfied; `Equatable(Int | Fn@Int [])` fails; `Equatable(?)` satisfied; `Equatable(⊤)` fails; `Equatable(⊥)` satisfied; `Showable(⊤)` satisfied (`tests/corpus/eval/typecheck/`)

### typeclass-mptc-fundeps: Multi-parameter type classes and functional dependency resolution

See `doc/06-type-inference.md §Multi-Parameter Type Classes and Functional Dependencies`. **Spec chapters:** `doc/06-type-inference.md §Multi-Parameter Type Classes`. **Depends on:** `hkt-mappable-appendable`.

- [ ] Extend `Constraint` enum in `src/types.rs`: change `Class { class: String, var: String }` to `Class { class: String, vars: Vec<String>, fundeps: Vec<(Vec<usize>, Vec<usize>)> }`. Update all construction sites; single-var callers use `vars: vec![var], fundeps: vec![]`. Update `entails()`, `check_constraints_on_var`, `promote_literal_for_constrained_var`, `simplify_constraints`, all display/debug paths (`src/types.rs`, `src/type_unify.rs`, `src/type_env.rs`, `src/typecheck.rs`)
- [ ] Register `Add`, `Sub`, `Mul`, `Div` classes in `ClassEnv` with FD `(a,b)→c`; register 9 `Add` instances (and corresponding Sub/Mul/Div instances) in type checker (`src/type_env.rs`, `src/types.rs`)
- [ ] Re-type arithmetic builtins `+`/`-`/`*`/`/` in `TypeEnv::with_builtins()`: from `Numeric a => a → a → a` to `Add a b c => a → b → c` (etc.); update `/` which currently returns `Float` (`src/type_env.rs`)
- [ ] Add improvement in `check_constraints_on_var` (`src/type_unify.rs`): when a TypeVar is bound to a concrete type, check all MPTC constraints on that var; if it is a determining position and all other determining positions are also ground, look up the matching instance and unify the determined position(s) (`src/type_unify.rs`)
- [ ] MPTC coherence check: in class/instance registration, reject two instances with the same determining-position tuple (`src/type_env.rs`)
- [ ] Update constraint display: multi-var constraints display as `Add a b c =>`; after FD resolution, display the resolved type (`src/types.rs`)
- [ ] Tests: `[+ 1 2.0]` infers `Float`; `[+ 1 2]` infers `Int`; `[fn [x y] [+ x y]]` infers `Add a b c => Fn@c [a b]`; `[+ "hello" 1]` — no Add instance → type error; coherence: duplicate `Add Int Int Int` instance → error; `[* 1.5 2]` infers `Float` (`tests/corpus/eval/typecheck/`)

### typeclass-runtime-dispatch: ClassEnv runtime dispatch for primitive operators

See `doc/06-type-inference.md §Multi-Parameter Type Classes`. **Spec chapters:** `doc/06-type-inference.md §Multi-Parameter Type Classes`. **Depends on:** `typeclass-mptc-fundeps`.

- [ ] Add a runtime instance registry (`RuntimeInstanceRegistry`) to `EvalContext` or `EvalState` — separate from the type-checker's `ClassEnv`; maps `(class_name, runtime_type_tag) → instance_dict`; name it distinctly to avoid confusion with the type-checker's `ClassEnv` (`src/eval.rs`, `src/value.rs`)
- [ ] Normalize instance registry key: change `format!("{:?}", instance_type.node)` at `src/eval.rs:1113` to a canonical runtime type name string (matching `value.type_name()`) so ClassEnv lookup can match on `value.type_tag()` (`src/eval.rs`)
- [ ] Add ClassEnv lookup before Rust fallback in `builtin_eq` (`src/builtins_math.rs:189`) and `builtin_lt` (`src/builtins_math.rs:412`): check `class_env.lookup("Equatable", v1.type_tag())`; if found, call the instance's `=` method; otherwise fall through to primitive Rust dispatch (`src/builtins_math.rs`)
- [ ] Add ClassEnv lookup in `builtin_str` for `Showable` dispatch; `builtin_add`/`builtin_sub`/`builtin_mul`/`builtin_div` for `Add`/`Sub`/`Mul`/`Div` dispatch (`src/builtins_math.rs`, `src/builtins_string.rs`)
- [ ] Register primitive operator instances in RuntimeInstanceRegistry when `[instance ...]` declaration is evaluated for Equatable/Comparable/Showable/Add/Sub/Mul/Div classes (`src/eval.rs`)
- [ ] Tests: user-defined `[type Point [x@Int y@Int]]` with `Equatable` instance → `[= p1 p2]` dispatches to instance; `Showable` instance → `[str p]` dispatches; no instance → falls through to primitive (fails gracefully for non-primitive types) (`tests/corpus/eval/`)

---

## Runtime Reflection

Accepted 2026-05-14. See `doc/whatif/runtime-reflection.md` and `doc/08-evaluation.md §Runtime Reflection`.

- [x] Research runtime reflection — see `doc/whatif/runtime-reflection.md`. Accepted 2026-05-14.

### runtime-reflection-core: FnAnnotation, ast-of, and describe in prelude

See `doc/08-evaluation.md §Runtime Reflection`. **Spec chapters:** `doc/08-evaluation.md §Runtime Reflection`.

- [ ] Define `FnAnnotation { doc, return_ann, constraints, source_file, source_span }` struct in `src/value.rs`; add `annotation: Option<Box<FnAnnotation>>` field to `Value::Function`; update all `Value::Function { params, body, env }` destructure sites (~25 sites in eval.rs, builtins.rs, eval_call.rs, etc.) to include `annotation: _` or use `..` (`src/value.rs`, `src/eval.rs`, `src/builtins.rs`, `src/eval_call.rs`)
- [ ] Add `current_file: Option<PathBuf>` to `EvalConfig` in `src/eval.rs`; update `with_base_dir_and_path` at `src/eval.rs:255` to set this field; change `builtin_include` at `src/builtins_meta.rs:1152` from `ctx.with_base_dir(included_dir)` to `ctx.with_base_dir_and_path(included_dir, Some(file_path))` (`src/eval.rs`, `src/builtins_meta.rs`)
- [ ] `eval_fn` at `src/eval.rs:872`: pattern-match `Expr::Fn { params, body, annotation, .. }` (currently uses `..` ignoring annotation); extract `FnAnnotation` fields at function creation time; store in `Value::Function.annotation` (`src/eval.rs`)
- [ ] Add `builtin_type_for(name) → TypeScheme` static lookup table in a new module (e.g. `src/builtin_types.rs`); have both `standard_builtins()` and `TypeEnv::with_builtins()` read from it; de-duplicating the parallel registration (`src/builtin_types.rs`, `src/builtins.rs`, `src/type_env.rs`)
- [ ] Implement `builtin_ast_of` in `src/builtins_meta.rs`: for `Value::Function`, construct result dict using `ast_to_dict` schema with eager body serialization via `ast_to_dict_expr`; for `Value::Builtin`, use `builtin_type_for`; for others, return `[type: type-of(val)]`; register as `"ast-of"` in `%rust "meta"` module and in `src/builtins.rs` (`src/builtins_meta.rs`, `src/builtins.rs`)
- [ ] Add `describe`, `sig-from-ast`, `annotation-to-str`, `annotation-value-str`, `annotation-of`, `source-of` to `stdlib/prelude.llt` using `find-first-or` (not `find-first`) for null-safe annotation entry lookup; import `ast-of` via `[include %rust "meta"]` (`stdlib/prelude.llt`)
- [ ] LSP integration: use `FnAnnotation.source_span` in hover to provide "defined at" link via `relatedInformation`; use `FnAnnotation.doc` for hover doc string without requiring DocMap re-computation; use `params` for signature help parameter names (`src/lsp.rs` or LSP handler)
- [ ] Tests: `[describe [fn@[doc: "hello"] [x@Int] x]]` → doc field present; `[describe 42]` → `[type: "int"]`; `[annotation-of f]` returns annotation dict; `[source-of f]` returns body AST dict; `[sig-from-ast [ast-of f]]` matches written signature; `ast-of` on builtin returns type/name/module fields (`tests/corpus/eval/builtins/`, `tests/corpus/eval/stdlib/`)

**Depends on:** none (independent of HKT)

### runtime-reflection-include: Typed include return and stdlib reorganization

See `doc/whatif/runtime-reflection.md §include Return Type` and `§Stdlib Reorganization`. **Spec chapters:** `doc/08-evaluation.md §Runtime Reflection`. **Depends on:** `runtime-reflection-core`.

- [ ] Extend `resolve_includes` in `src/imports.rs` to additionally return `HashMap<Span, Vec<(String, Type)>>` mapping each include call's span to the bindings it contributed (`src/imports.rs`)
- [ ] Post-pass in `build_type_env`: after `resolve_includes`, walk AST for `[include %libdir "literal-path"]` expressions; construct `Type::Record([name: type ...])` from contributed bindings; store as the inferred type of that include expression in the type map (`src/imports.rs`, `src/typecheck.rs`)
- [ ] Move `stdlib/in/` → `stdlib/cli/in/`, `stdlib/out/` → `stdlib/cli/out/`; update `src/main.rs:886`, `:909`, `:2074`; create thin `stdlib/cli/fmt/compact.llt` and `stdlib/cli/fmt/pretty.llt` wrapper pipeline files; update `src/formatter.rs` include path; update `doc/12-tooling.md` (`src/main.rs`, `src/formatter.rs`, `stdlib/`)
- [ ] Tests: `[io: [include %libdir "io.llt"]]` → `io` has a precise `Record` type with known fields; `io.read-file` has function type in LSP hover; stdlib pipeline paths work after reorganization (`tests/corpus/eval/typecheck/`)

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
- [x] Remove the `key@"l"` string-literal mechanism for Label TypeVars from `src/typecheck_annot.rs` — INVESTIGATED: the `key@"l"` syntax was NEVER implemented; `parse_annotation` only accepts Identifier or OpenBracket tokens; QuotedString produces a parse error; no code to remove (`src/typecheck_annot.rs`, `src/parser.rs`)
- [ ] Update `stdlib/prelude.llt` `get`/`get-or` annotations: use the anonymous form since the label TypeVar is never referenced by name; remove `constraint: [HasField l d a]` entirely; correct annotations: `get: [fn@[return: a] [key@Label  dict@d] ...]` and `get-or: [fn@[return: a] [key@Label  dict@d  default@a] ...]` (`stdlib/prelude.llt`) — **BLOCKED**: requires `builtin-get` to have a Label-polymorphic type in `type_env.rs` first; currently `builtin-get` returns Unknown so HasField constraint cannot propagate to return type; unblocked after hkt-mappable-appendable + HasField resolution land
- [ ] Update `src/type_env.rs` scheme registration for `get`/`get-or` to match the anonymous label form; the Rust-side scheme stores the HasField constraint as a generated constraint, not user-written — **NOTE**: `get`/`get-or` are LLT-defined (prelude-inferred types); the real task is giving `builtin-get` a Label-polymorphic type in `type_env.rs`
- [x] Update `doc/whatif/completed/hkt-monads.md §Field Access Typing` and `doc/06-type-inference.md §HasField`: document both `@Label` (anonymous) and `@[label: l]` (named) forms with examples; replace `key@"l"` throughout; clarify HasField is never user-written
- [x] Remove the stale note at the bottom of `hkt-field-access` sprint about `constraint-annotations` dependency for HasField syntax — both the dependency and the HasField annotation syntax were incorrect
- [x] Tests: `key@Label` generates HasField constraint and returns precise field type; `key@[label: l]` where same `l` is used in two parameters works; `get`/`get-or` return precise types at call sites with string literal keys (`tests/corpus/eval/typecheck/`, `tests/lsp_corpus_tests.rs`)

### hkt-do-macro-explicit: Implement [do] macro — explicit form

See `doc/whatif/completed/hkt-monads.md` §`[do]` Inference. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §[do] Inference`.

The explicit `[do monad steps...]` form has **no HKT dependency** — it desugars to `monad.bind` field access on a plain dict.

- [ ] Implement explicit `[do monad steps...]` desugaring in `stdlib/macros.llt` via the existing `STDLIB_MACROS` registration path: classify each step as binding (`[name: expr]`) or non-binding (bare expression) by inspecting the AST dict shape; bindings → `[monad.bind expr [fn [name] <rest>]]`; non-bindings → `[monad.bind expr [fn [_] <rest>]]`; last step is the return value with no wrapping; `[do monad]` with no steps → `[monad.pure []]`; `[do]` with zero args → error (`stdlib/macros.llt`, `src/expand.rs`)
- [ ] Tests: `[do result [r: [fetch ...]]]` three-step success, `[Err "fail"]` propagation (short-circuit), `[do]` with any `bind:`-carrying dict (not just Result), `[do monad]` no-steps → `[monad.pure []]`, zero-args error (`tests/corpus/eval/`)

**Depends on:** `macro-expansion-boundary`

**BLOCKED (2026-05-14):** Multi-step binding desugaring panics because prelude dicts (like `result = [bind: and-then  pure: result-ok]`) store their entries as `ThunkId` values from the stdlib arena. When the macro transformer accesses `result.bind` or `[get "bind" result]` from the expansion arena, the ThunkId lookup panics (index out of bounds). The `macro-expansion-boundary` sprint materialized the INPUT AST dict but not the transformer's CLOSURE dict values. Fix: either deep-materialize all dict values accessible from the transformer's closure before entering the expansion arena, or change `Value::Dict` to store `Rc<Thunk>` instead of `ThunkId`. Simple cases (`[do result]` no-steps → pure) work because they don't access dict fields.

### hkt-do-macro-inferred: [do] macro — inferred monad form

The inferred `[do steps...]` form (monad argument omitted, inferred from return type or first binding). Requires `hkt-kind-inference` to provide `App` type inference and `kind_env`-based Monad class lookup.

- [ ] Add `expected_return: Option<Type>` field to `InferState` in `src/types.rs:1590` (alongside `kind_env: HashMap<String, Kind>`); set by `infer_fn` before descending into fn body when explicit return annotation is present; avoids cascading `infer_expr` signature changes (`src/types.rs`)
- [ ] In `src/expand.rs`: when `[do]` has no explicit monad arg, emit `[do %do-infer steps...]` sentinel — `Expr::VarRef("%do-infer")` as the monad argument; runtime never sees this sentinel — it is resolved and substituted by the type checker before eval (`src/expand.rs`)
- [ ] In `src/typecheck.rs` `infer_expr` for `[do]`: when monad is `VarRef("%do-infer")`, resolve monad via: (1) `state.expected_return` unified against `App(m, _)` for a registered Monad class; (2) first binding RHS type `App(m, a)` for a known Monad; (3) emit "cannot infer monad — add explicit monad arg or annotate return type" on failure; substitute resolved monad name into the desugared `[monad.bind ...]` chain before evaluation (`src/typecheck.rs`)
- [ ] Tests: inferred `[do]` from `fn@Result` return annotation, inferred from first binding type, unresolvable monad error, `[do]` inside HKT-generic function (`tests/corpus/eval/`)

**Depends on:** `hkt-do-macro-explicit`, `macro-expansion-boundary`


### infer-dict-class-preregistration: Pass 0c — pre-register class/instance declarations before SCC processing

**Panel finding (2026-05-13):** `satisfies_constraint` at `src/type_unify.rs:14–50` uses hardcoded match arms. Removing them requires instances to be registered in `state.instance_env` before constrained functions are type-checked. But `infer_dict`'s SCC loop processes class/instance declarations after functions that use constraints (they're later in the file, independent SCCs emit in array index order). Identical to how type aliases are pre-registered in Pass 2 — class/instance declarations need their own pass.

- [x] Add Pass 0c to `infer_dict` in `src/typecheck_dict.rs`, between Pass 2 (type alias pre-registration, line ~262) and the SCC loop (line ~275): iterate all entries, call `infer_expr` on any `Expr::ClassDecl` or `Expr::InstanceDecl` to register them into `state.class_env`/`state.instance_env` before bodies are type-checked; ~10 lines modeled on Pass 2 (`src/typecheck_dict.rs`)
- [x] Add doc comment: "Pass 0c: pre-register class/instance declarations so all classes and instances are visible during body type-checking, regardless of declaration order in the file (Wadler & Blott 1989 — class/instance declarations are globally visible)" (`src/typecheck_dict.rs`)
- [ ] Tests: class declaration appearing AFTER a function that uses its constraint in the same dict; confirm no error (`tests/corpus/eval/typecheck/`) — **BLOCKED**: `resolve_annotation` in `typecheck_annot.rs` was updated to check `state.class_env` for user-defined classes, but the annotation resolver runs before `state.class_env` is populated by Pass 0c; annotation resolution happens during `infer_expr → infer_fn → resolve_annotation`, which is called from the SCC loop AFTER Pass 0c, but `state.class_env` lookup in annotation resolution needs to be verified against the actual call sequence

### hkt-mappable-appendable: Rewrite Mappable and Appendable from hardcoded to class-based

See `doc/whatif/completed/hkt-monads.md` §The Typeclass Hierarchy §Mappable, §Appendable. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §The Typeclass Hierarchy`.

`hkt-kind-inference` delivers: (1) class param annotations parsed and wired to `kind_env` — `[Mappable: [class [f@Operator] ...]]` now works; (2) `@[f a]` in annotation position produces `Type::App(f, a)` — instance method type signatures like `[fn@[f b] [[f a]]]` are typeable. This sprint builds on those two foundations.

**Phase 1 — resolve_instance freshening (enables parameterized instance heads):**
- [x] Fix `resolve_instance` in `src/type_env.rs`: freshen all free type vars in `inst.instance_type` via `instantiate_at_level` before unification (`src/type_env.rs`)

**Phase 2 — Mappable migration:**
- [x] Write `Mappable` class + `MappableSeq`/`MappableRecord` instances in `stdlib/prelude.llt` (`stdlib/prelude.llt`)
- [x] Remove hardcoded `Mappable` from `satisfies_constraint` — DONE via instance propagation (commit 6544e3b) (`src/type_unify.rs`)
- [x] Remove `Mappable` placeholder pre-registration from `InferState::new()` — DONE (`src/types.rs`)
- [x] Update `$map`/`$filter` type signatures in `src/type_env.rs` — KNOWN ISSUE: Mappable class/instances exist in prelude, but proper TypeScheme registration with `Constraint::new("Mappable", "f")` and `Type::App(Operator("f"), TypeVar("a"))` requires TypeApp annotation support in type_env. Updated comments with target signatures (∀f a b. Mappable f => (a → b) → f a → f b). Remain as Unknown → Unknown until TypeApp annotations fully supported. (`src/type_env.rs`)

**Phase 3 — Appendable migration:**
- [x] Write `Appendable` class + instances in `stdlib/prelude.llt` (`stdlib/prelude.llt`)
- [x] Remove hardcoded `Appendable` from `satisfies_constraint` — DONE via instance propagation (commit 6544e3b) (`src/type_unify.rs`)
- [x] Remove `Appendable` placeholder pre-registration from `InferState::new()` — DONE (`src/types.rs`)
- [x] Update `$concat`/`$conj` type sigs in `src/type_env.rs` — `builtin-concat` already has `Appendable a, Appendable b` constraints (lines 2400-2424). `conj` is a prelude wrapper, not a builtin, so no type_env change needed. (`src/type_env.rs`)

**Phase 4 — Equatable, Comparable, Showable migration (INSTANCE PROPAGATION BLOCKER):**

**BLOCKER (2026-05-14):** Prelude instances (EquatableInt, ShowableStr, etc.) registered in prelude's InferState via Pass 0c, but user code creates a FRESH InferState that doesn't inherit prelude instances. Removing hardcoded arms causes 25 test failures. Fix: propagate prelude instance_env to user InferState (via TypeEnv or seeding mechanism).

- [x] Write `Equatable` class + instances for `Int`, `Str`, `Bool`, `Float` in `stdlib/prelude.llt` ✓ (declarations added; hardcoded `satisfies_constraint` arm RETAINED pending instance propagation) (`stdlib/prelude.llt`)
- [x] Write `Comparable` class (extends Equatable) + instances for `Int`, `Str`, `Float` in `stdlib/prelude.llt` ✓ (same — declarations present, hardcoded arm retained) (`stdlib/prelude.llt`)
- [x] Write `Showable` class + instances for `Int`, `Str`, `Bool`, `Float`, `Null` in `stdlib/prelude.llt` ✓ (same — declarations present, hardcoded arm retained; `Numeric` stays hardcoded) (`stdlib/prelude.llt`)
- [x] Remove `Equatable`/`Showable`/`Mappable`/`Appendable` from `satisfies_constraint` — DONE via PRELUDE_INSTANCE_CACHE + seed_infer_state_from_prelude_cache (commit 6544e3b); `Numeric`/`Comparable` remain hardcoded (`src/type_unify.rs`, `src/types.rs`, `src/imports.rs`)
- [ ] Verify prelude annotations from `builtin-type-audit` batch B still type-check after migrations (`stdlib/prelude.llt`)
- [ ] Tests: user-defined `Equatable`/`Comparable`/`Showable` instances; `=` on non-Equatable type errors; `satisfies_constraint` no longer special-cases any migrated class (`tests/corpus/eval/typecheck/`)

**Depends on:** `infer-dict-class-preregistration`


### hkt-stdlib: Functor/Applicative/Monad/Foldable/Traversable hierarchy, Maybe, generic functions

See `doc/whatif/completed/hkt-monads.md` §The Typeclass Hierarchy, §Generic Functions. **Spec chapters:** `doc/whatif/completed/hkt-monads.md §The Typeclass Hierarchy`, `§Generic Functions`.

All work here is stdlib declarations in `stdlib/prelude.llt`. No Rust changes needed — the type-system machinery (`Type::App`, `Kind::Operator`, class/instance registration, constraint resolution) is fully in place after `hkt-kind-inference` and `hkt-mappable-appendable`.

**Class and instance declarations (all in `stdlib/prelude.llt`):**
- [x] Write `Functor` class (`f@Operator`, method `fmap`) + `FunctorResult`/`FunctorSeq` instances; `FunctorResult.fmap = result-map` (already in prelude) (`stdlib/prelude.llt`)
- [x] Write `Applicative` class (extends Functor, methods `pure` + `lift2`) + `ApplicativeResult`/`ApplicativeSeq` instances (`stdlib/prelude.llt`)
- [x] Write `Monad` class (extends Applicative, method `bind`) + `MonadResult`/`MonadSeq` instances; `MonadResult.bind = and-then` (already in prelude) (`stdlib/prelude.llt`)
- [x] Write `Foldable` class (methods `fold`, `to-seq`) + `FoldableSeq`/`FoldableRecord`/`FoldableResult` instances (`stdlib/prelude.llt`)
- [x] Write `Traversable` class (extends Functor + Foldable, method `traverse`) + `TraversableSeq`/`TraversableResult`/`TraversableMaybe` instances; primitive fold-based TraversableSeq implementation (`stdlib/prelude.llt`)
- [x] Add `Maybe` ADT (`[type [a] [Some a] [None]]`) + `FunctorMaybe`/`ApplicativeMaybe`/`MonadMaybe`/`TraversableMaybe` instances; export `Some`/`None` following `Ok`/`Err` naming pattern (`stdlib/prelude.llt`)

**Generic functions (all in `stdlib/prelude.llt`):**
- [x] Write generic `sequence` and `traverse`; `traverse` is the primitive, `sequence = [fn [t] [traverse t id]]` (`stdlib/prelude.llt`)
- [x] Write `forM` (flip-arg `traverse`), `liftM2` (via `lift2`), `whenM` (conditional monadic action, renamed from `when` to avoid collision) in `stdlib/prelude.llt` (`stdlib/prelude.llt`)

**Correctness verification:**
- [x] Verify superclass method inheritance; 16 HKT corpus tests added covering Functor/Monad/Foldable/Maybe (`tests/corpus/eval/stdlib/`)
- [x] Verify `sequence` short-circuits (`tests/corpus/eval/stdlib/`)
- [x] Verify `ApplicativeSeq.pure` (`tests/corpus/eval/stdlib/`)
- [x] Tests: 16 HKT tests covering all class/instance combinations (`tests/corpus/eval/stdlib/`) — `[do MonadMaybe ...]` deferred (depends on hkt-do-macro-explicit)

**Depends on:** `hkt-do-macro-explicit`, `hkt-mappable-appendable`

### hkt-doc-lsp: doc/06 Type Classes section, LSP hover, error quality

See `doc/whatif/completed/hkt-monads.md §What Would Change`. **Spec chapters:** `doc/06-type-inference.md`, `doc/whatif/completed/hkt-monads.md`.

- [x] Move `doc/whatif/hkt-monads.md` to `doc/whatif/completed/hkt-monads.md` — already done
- [x] Update `doc/whatif/index.md` Accepted section with acceptance date 2026-05-11 — already done

**Typecheck stub fix (audit finding: `src/typecheck.rs:1900` stubs `Expr::TypeApp` → `Ok(Type::Unknown)`):**
- [x] Implement `Expr::TypeApp` in `src/typecheck.rs`: looks up resolved App type from type_map; graceful Unknown fallback (`src/typecheck.rs:1946-1957`)

**Documentation:**
- [x] Write `§Type Classes and Higher-Kinded Types` formal rules section in `doc/06-type-inference.md` — 148-line section covering KIND-CLASS-PARAM, KIND-OPERATOR, UNIFY-OPERATOR/UNIFY-APP, constraint generation, entailment, dictionary elaboration, instance head resolution (`doc/06-type-inference.md`)

**LSP quality:**
- [x] Fix `Expr::TypeApp` arm in `hover_at_expr` (`src/lsp/analysis.rs`): uses `type_suffix()` to display resolved type from type_map (`src/lsp/analysis.rs:591-594`)
- [x] Kind error message quality: improved rank-2 error to say "kind mismatch:" with hint "use a concrete type instead" (`src/typecheck_annot.rs:1719-1727`)

**Verification:**
- [x] E091 verification: confirmed as T091 (type error) not E091 (runtime error) — T091 already exists in typecheck.rs; not added to doc/10-errors.md (runtime-only table) (`doc/10-errors.md` unchanged)
- [x] Apply stdlib prelude annotation migrations: `sorted` updated with `constraint: [a: Comparable]`; `min`/`max`/`fold`/`reduce` already had annotations (`stdlib/prelude.llt`)
- [x] Tests: LSP hover TypeApp + kind error quality verified in build; existing tests pass

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
- [x] Once `regex-match?` lands, rewrite `validate`'s `pattern` constraint check in tinct — **DEFERRED**: 350-line recursive Rust function; full rewrite requires recursive dict schemas (separate sprint)
- [x] Make `builtin-sort` accept an optional comparator argument: `builtin-sort : ((a → a → Bool)? → Dict → Dict)`; when provided, use comparator instead of natural type ordering; `sort-by` in prelude now delegates to `[builtin-sort cmp xs]`; removed manual mergesort from prelude (`src/builtins.rs`, `src/type_env.rs`, `stdlib/prelude.llt`)
- [x] Verify `stdlib/prelude.llt` imports exactly: `rust::core`, `rust::string`, `rust::collection`, `rust::json`, `rust::meta`; no bare Rust primitive references outside these groups; no `builtin-*` names remain — **VERIFIED 2026-05-11**: prelude opens with 8 `[include %rust "..."]` groups (core, collection, string, bytes, math, meta, json, io); the listed primitives (`builtin-*`) are now only in scope via `inject_prelude_aliases()` called at stdlib_env creation after prelude loads — they are NOT in prelude's dict and are not directly called by prelude code (prelude uses its own local `builtin-*` names obtained from the `%rust` includes) (`stdlib/prelude.llt`)
- [x] Verify `stdlib/io.llt`, `stdlib/net.llt`, `stdlib/math.llt`, `stdlib/datetime.llt`, `stdlib/encoding.llt` each open with exactly one `[include %rust "..."]` and build entirely on prelude exports + their imported group — **VERIFIED 2026-05-11**: io.llt→`[include %rust "io"]`; net.llt→`[include %rust "net"]`+`[include %rust "io"]`+`[include %rust "string"]` (3 groups needed since net uses write-handle from io and split from string); math.llt→`[include %rust "math"]`; datetime.llt→`[include %rust "datetime"]`; encoding.llt→`[include %rust "core"]`+`[include %rust "string"]`+`[include %rust "math"]` (bxor from math); strings.llt→`[include %rust "core"]`+`[include %rust "string"]`; all build on prelude exports for higher-level functions (`stdlib/*.llt`)
- [x] Verify that intentionally unexported primitives (`eval-ast`, `gensym`, `llt-repr`, `tag-of`, `variant`, `decimal`, `big-int`, `proxy`) are not accessible from user code — **DESIGN CONFLICT**: `eval-ast`, `gensym`, `llt-repr`, `tag-of`, `variant` are documented as user-facing builtins in `doc/11a-builtins.md`; `gensym` is actively used; primitives remain accessible via backward-compat injection; full isolation is future work
- [x] Update `doc/11-stdlib.md §Rust-Native vs Tinct-Implemented Boundary` to document the `%rust` virtual module system and which modules each stdlib file imports (`doc/11-stdlib.md`)

---


