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

## Feature Doc Verification

Three-way review of `doc/feature/*.md` against `doc/*.md` and source code. Each item names the file to change and what to change. Source-of-truth decisions follow the batch agents (feature doc wins unless code clearly implements something else).

### fdv-feature-doc-updates: Correct stale content in doc/feature/ files

For findings where the code is the source of truth and the feature doc is wrong.

- [ ] `doc/feature/parser-rewrite.md` §Implementation — change every reference to `src/parser2.rs` to `src/parser.rs`; remove the phrase "replacing `src/parser.rs`" (the rewrite landed in-place, no rename) (`doc/feature/parser-rewrite.md`)
- [ ] `doc/feature/parser-rewrite.md` §Lexer, §Formatter, §Overview benefit #4 — remove all `Token::BracketAccess` references; note that bracket-access syntax (`$a[0]`) was evaluated and removed; `[` always emits `OpenBracket` (`doc/feature/parser-rewrite.md`)
- [ ] `doc/feature/arena-patterns.md` §Implementation — clarify that the §Design section describes the full Phase 3 target; the current implementation is Phase 2 (arena stores `Rc<Thunk>`, not direct ownership); update the migration table, `BuiltinFn` snippet, and "All phases implemented" header claim to reflect Phase 2 reality (`doc/feature/arena-patterns.md`)
- [ ] `doc/feature/circular-dep-error-paths.md` §Design — change `include_guard: HashSet<PathBuf>` → `HashSet<(u64, u64)>` and `include_cache: HashMap<PathBuf, Rc<Thunk>>` → `HashMap<(u64, u64), Rc<Thunk>>`; add a note that the guard uses `(device, inode)` file identity to avoid TOCTOU races (`doc/feature/circular-dep-error-paths.md`)
- [ ] `doc/feature/circular-dep-error-paths.md` §Performance Cost — remove the `EvalConfig.track_cycle_path: bool` and `--no-cycle-track` text; the push/pop overhead is unconditional and no flag exists (`doc/feature/circular-dep-error-paths.md`)
- [ ] `doc/feature/source-text-availability.md` §REPL Integration — remove the `render_eval_error` wrapper function (it does not exist); show the actual inline pattern used: `format!("{e}")` + `render_span_snippet(input, e.definition_span)` (`doc/feature/source-text-availability.md`)
- [ ] `doc/feature/string-interpolation.md` §Implementation §Parser — replace the `desugar_interpolated_string()` description with the actual two-step flow: parser calls `emit_tmpl_call()` → `[tmpl "raw-template" expr0...]`; `tmpl-transformer` in `stdlib/macros.llt` expands `[tmpl ...]` to `[str ...]` at macro-expansion time (`doc/feature/string-interpolation.md`)
- [ ] `doc/feature/string-interpolation.md` §Formatter — replace "they render as `[str ...]` calls" with a description of the formatter's `i"..."` reconstruction heuristic in `src/formatter.rs:950-1056` (`doc/feature/string-interpolation.md`)
- [ ] `doc/feature/string-interpolation.md` §Internal Representation — add `InterpolatedPart::Expr(String)` to the enum; update §Semantics to describe the two desugaring paths: `$name` → VarRef encoded in template string; `${expr}` → `Expr(raw)` re-parsed and passed as extra arg (`doc/feature/string-interpolation.md`)
- [ ] `doc/feature/access-pipeline.md` §Generator Primitives — change `get : Key → Record → Any` to `get : (Key, Record) → Any`; remove "curried" description; add note that pipe appends `dict` as second arg via desugar rule, no currying required (`doc/feature/access-pipeline.md`)
- [ ] `doc/feature/null-semantics.md` §Implementation §Type Checker — change `src/typecheck.rs` to `src/typecheck_annot.rs` for `resolve_type_name` (`doc/feature/null-semantics.md`)
- [ ] `doc/feature/null-semantics.md` §Implementation §Builtin Type Signatures — change `src/types.rs` to `src/type_env.rs` (`doc/feature/null-semantics.md`)
- [ ] `doc/feature/null-semantics.md` §Implementation §env Builtin — replace "retains `Type::Any` until union types provide `String | Null`" with a statement that `env` is already registered as `Union(String, Null)` (`doc/feature/null-semantics.md`)
- [ ] `doc/feature/null-semantics.md` — replace all occurrences of `Type::Record(Row::Empty)` with `Type::Record(Row { fields: HashMap::new() })` or prose "a closed record with no fields" (there is no `Row::Empty` variant; `Row` is a struct) (`doc/feature/null-semantics.md`)
- [ ] `doc/feature/let-binding.md` §Implementation §AST — state that `Expr::Sequential(Vec<Rc<Spanned<Expr>>>)` is a first-class AST variant added to `src/ast.rs`, with explicit arms in the evaluator, type checker, formatter, expander, and LSP; remove the claim that no AST changes were needed (`doc/feature/let-binding.md`)
- [ ] `doc/feature/let-binding.md` §Design §Rationale §4 — note that sequential arm bodies in `[match]` are planned but not yet implemented; mark the multi-body match arm syntax example as "planned" and cross-reference the `multi-body-positions` sprint (`doc/feature/let-binding.md`)
- [ ] `doc/feature/pattern-matching.md` — verify `src/parser.rs` match arm parsing to confirm whether the surface syntax uses `:` as separator or space; update examples in `doc/feature/pattern-matching.md` to accurately reflect the actual separator used (colon vs space) (`doc/feature/pattern-matching.md`, `src/parser.rs`)
- [ ] `doc/feature/type-predicates.md` §Design — add `bytes?`, `record?`, `map?`, and `seq?` to the predicate table (code registers 13 predicates; the feature doc lists only 8) (`doc/feature/type-predicates.md`)
- [ ] `doc/feature/type-predicates.md` §Type Checker Integration — change cross-reference from `doc/whatif/narrowing.md Pattern 2` to `doc/06-type-inference.md §Type Narrowing` (`doc/feature/type-predicates.md`)
- [ ] `doc/feature/quasiquoting.md` §Overview — note that nested `[unquote ...]` in non-top-level positions (inside call args, dict values, seq literals) is not yet implemented; mark nested-unquoting examples as "planned"; cross-reference the `eval-gaps` sprint (`doc/feature/quasiquoting.md`)
- [ ] `doc/feature/quasiquoting.md` §Design §AST Dict Schema — change `doc/whatif/ast-schema.md` to `doc/whatif/completed/ast-schema.md` (file was moved) (`doc/feature/quasiquoting.md`)
- [ ] `doc/feature/union-types.md` §"Interaction with `Any`" (both occurrences) — replace `Any` with `Unknown`/`Top` as appropriate; note the `gradual-typing-split` sprint is already complete (`doc/feature/union-types.md`)
- [ ] `doc/feature/union-types.md` §Annotation-Only Unions — qualify "unify never produces unions" to note that `infer_if` branch joins do produce union types via `Type::normalize_union`; the statement applies only to `unify` proper (`doc/feature/union-types.md`)
- [ ] `doc/feature/union-types.md` §Full Algebraic Subtyping — replace the Simple-sub description with the BAS design that was actually implemented (Chau & Parreaux POPL 2026); retitle the section; update the Parreaux (2020) reference note (`doc/feature/union-types.md`)
- [ ] `doc/feature/algebraic-data-types.md` — add a note at the top of §Design stating that the structural ADT approach is superseded by BAS; structural record unions collapse to Top under S-RcdTop; discriminated ADTs require nominal variants (`doc/feature/algebraic-data-types.md`)
- [ ] `doc/feature/algebraic-data-types.md` §Builtins — replace the structural Result type `Union([ok: a], [err: Str])` with the nominal `Ok[a] | Err[String]` form; reference `doc/feature/nominal-variants.md` (`doc/feature/algebraic-data-types.md`)
- [ ] `doc/feature/algebraic-data-types.md` §Type Checker — note that multi-entry `[type ...]` is handled via `resolve_type_expr`/`resolve_type_dict` in `src/typecheck_annot.rs`, not `infer_dict` (`doc/feature/algebraic-data-types.md`)
- [ ] `doc/feature/algebraic-data-types.md` §Implementation — verify `Expr::Str` handling in `resolve_type_expr` (`src/typecheck_annot.rs`); if the `Expr::Str(s) => Ok(Type::StringLiteral(s.clone()))` arm is missing, add it; if present, confirm and document (`doc/feature/algebraic-data-types.md`, `src/typecheck_annot.rs`)
- [ ] `doc/feature/nominal-variants.md` §Runtime Value — change `payload: Option<Rc<Thunk>>` to `payload: Option<ThunkId>`; add a note that ThunkId is an arena handle following the same pattern as `Value::Dict` and `Value::Seq` (`doc/feature/nominal-variants.md`)
- [ ] `doc/feature/nominal-variants.md` §Grammar — replace "In `src/grammar.pest`" with "In `src/parser.rs`" throughout (project uses a hand-written iterative descent parser, no `grammar.pest` file exists) (`doc/feature/nominal-variants.md`)
- [ ] `doc/feature/nominal-variants.md` §Implementation — note that `Value::Variant` (runtime) is implemented but `Type::NominalVariant` (type-level) is not yet added; distinguish completed from pending work (`doc/feature/nominal-variants.md`)
- [ ] `doc/feature/nominal-variants.md` §AST — mark `Pattern::Constructor` as implemented (already in `src/ast.rs:331-334`) (`doc/feature/nominal-variants.md`)
- [ ] `doc/feature/parameterized-type-aliases.md` §Overview item 3 — mark partial application of type aliases as "not yet implemented"; arity is currently exact only; remove or qualify the `[Mapper Int]` partial-application example (`doc/feature/parameterized-type-aliases.md`)
- [ ] `doc/feature/parameterized-type-aliases.md` §Parser — review `src/parser.rs` TypeAlias parsing to confirm the parameter-list detection heuristic matches the feature doc's description ("only lowercase bare words"); update if different (`doc/feature/parameterized-type-aliases.md`, `src/parser.rs`)
- [ ] `doc/feature/narrowing.md` §Limitations item 1 and §Environment Forking — replace "false branch gets the original unrefined environment" with accurate description: TypeOf predicate narrowings produce `Negation(T)` in the false branch; EqLiteral and HasKey narrowings do not (`doc/feature/narrowing.md`)
- [ ] `doc/feature/narrowing.md` §Pattern 2 — replace `Type::Any` with `Type::Unknown` or `Type::Top` throughout; note `fn?` is handled as a direct predicate narrowing, not via `type-of` string match; `Type::Seq(Any)` → `Type::Seq(Unknown)` (`doc/feature/narrowing.md`)
- [ ] `doc/feature/narrowing.md` §Pattern 2 — remove the conditional "When type predicates are available" clause; describe `int?`, `str?`, `bool?`, `float?`, `seq?`, `num?`, `dict?`, `null?`, `fn?` as implemented direct narrowing patterns in `extract_narrowings` (`doc/feature/narrowing.md`)
- [ ] `doc/feature/narrowing.md` §Implementation — replace `narrow(Γ, cond, polarity)` with `apply_narrowings` / `apply_negation_narrowings` to match actual function names in `src/typecheck.rs` (`doc/feature/narrowing.md`)
- [ ] `doc/feature/narrowing.md` §Stdlib Narrowing / §Limitations item 2 — verify in `src/typecheck.rs` whether `[match]` arm typing calls `extract_narrowings`; if so, update the limitation to include `[match]` in the list of narrowed constructs (`doc/feature/narrowing.md`, `src/typecheck.rs`)
- [ ] `doc/feature/typeclasses.md` §Required Classes — remove `Null` from the Equatable instance list (no `Type::Null` variant exists; null is represented as the empty closed record); add `Number` which IS in `satisfies_constraint` but absent from the list (`doc/feature/typeclasses.md`)
- [ ] `doc/feature/typeclasses.md` §Required Classes and §Phase 1 — remove `Filterable` (methods: `filter`) and `Foldable` (methods: `reduce`, `length`) from the hardcoded constraint set; they are stdlib-declared classes, not primitive built-in constraints (`doc/feature/typeclasses.md`)
- [ ] `doc/feature/typeclasses.md` §Type Representation TypeScheme block — remove `row_vars: Vec<String>` (removed under BAS); add `label_vars: Vec<String>` and `doc: Option<String>` to match `src/types.rs:1404-1415` (`doc/feature/typeclasses.md`)
- [ ] `doc/feature/typeclasses.md` §Equatable for Records — mark `deep-eq` and `shallow-eq` as not yet implemented (neither exists in `src/builtins.rs`, `stdlib/prelude.llt`, or any source file) (`doc/feature/typeclasses.md`)
- [ ] `doc/feature/gradual-typing.md` §Implementation §Evaluator — clearly mark Phase 3b (automatic guard insertion at every `Unknown → Concrete` boundary) as future/unimplemented; current implementation is Phase 3a only (explicit blame at TypeAssert sites) (`doc/feature/gradual-typing.md`)
- [ ] `doc/feature/gradual-typing.md` §Consistency Relation — add `(Top, _) | (_, Top) => true` arm to the `is_consistent` pseudocode before the `_ => false` fallthrough (matches `src/types.rs:531`) (`doc/feature/gradual-typing.md`)
- [ ] `doc/feature/structural-contracts.md` §CLI — mark `tinct check` pipeline subcommand as not yet implemented (subcommand does not exist in `src/main.rs`) (`doc/feature/structural-contracts.md`)
- [ ] `doc/feature/structural-contracts.md` §CLI — add a note that `tinct describe` reads static annotations without full type inference (the subcommand exists but is narrower than the feature doc implies) (`doc/feature/structural-contracts.md`)
- [ ] `doc/feature/boolean-algebraic-subtyping.md` §Implementation Status — note that `find_compatible_member()` (correlates patterns to union members in `infer_match`) is not yet implemented; mark it as forward-looking (`doc/feature/boolean-algebraic-subtyping.md`)
- [ ] `doc/feature/hkt-monads.md` §Kind Annotations table — remove `key@"l"` (string-literal syntax rejected in `label-annotation-syntax` sprint); replace with `key@Label` (anonymous) and `key@[label: l]` (named) forms (`doc/feature/hkt-monads.md`)
- [ ] `doc/feature/hkt-monads.md` §Implementation section — update `[do]` stub status: the inferred `[do]` form is not yet implemented (requires `hkt-do-macro` sprint); explicit `[do monad steps...]` is backward-compatible and implemented (`doc/feature/hkt-monads.md`)
- [ ] `doc/feature/hkt-monads.md` §Generic Functions — add an implementation note that `sequence` and `traverse` are not yet in `stdlib/prelude.llt`; they require the `hkt-stdlib` sprint (`doc/feature/hkt-monads.md`)
- [ ] `doc/feature/hkt-monads.md` §Formal Type Rules — add a subsection describing `TypeScheme.label_vars: Vec<String>` and the requirement that `instantiate_scheme` must re-register each label var in `kind_env` with `Kind::Label` to prevent promotion-suppression failures after generalization (`doc/feature/hkt-monads.md`)
- [ ] `doc/feature/macros.md` §Implementation/Parser-Grammar and §Expansion-Pipeline — change `[defmacro name [params] body]` to `[defmacro name fn]`; change `Expr::DefMacro { name, params, body }` to `Expr::DefMacro { name, transformer }`; update description to say the transformer is a function expression passed directly (`doc/feature/macros.md`)
- [ ] `doc/feature/macros.md` §Expansion-Pipeline — rename `quote_macros` step to `expand_macros`; add `desugar` step after `expand_macros` to match the actual pipeline order (`doc/feature/macros.md`)
- [ ] `doc/feature/ast-schema.md` §The-Two-Rust-Functions — replace the `AstToDictOpts` struct with the three-field `CommentMaps<'a>` form; change `HashMap` to `BTreeMap`; add `ctx: &Rc<EvalContext>` parameter; note return type is `EvalResult<Rc<Thunk>>` not `Value` (`doc/feature/ast-schema.md`)
- [ ] `doc/feature/ast-schema.md` §Dict — remove `leading-comments: []` and `trailing-comment: []` from the "Keyed entry" example; the convention is to omit these fields entirely when empty (the `§Comment-fields-on-entries` description is already correct) (`doc/feature/ast-schema.md`)
- [ ] `doc/feature/tinct-hosted-formatter.md` §Implementation — rename `stdlib/formatter/format.llt` to `stdlib/formatter/pretty.llt` throughout; update implementation status to reflect that `pretty.llt` is implemented (`doc/feature/tinct-hosted-formatter.md`)
- [ ] `doc/feature/tinct-hosted-formatter.md` §Implementation / `src/main.rs` section — document the `--tinct-fmt` flag that opts into the tinct-hosted formatter; without this flag, compact modes use the Rust formatter (`doc/feature/tinct-hosted-formatter.md`)
- [ ] `doc/feature/macro-rewrite.md` §stdlib/macros.llt — clarify that `macros.llt` currently uses the `*-transformer` naming convention pre-registered via `STDLIB_MACROS` in `src/expand.rs`; `[defmacro ...]` declarations are pending the `stdlib-defmacro` sprint (`doc/feature/macro-rewrite.md`)
- [ ] `doc/feature/macros-cluster.md` §Sprint-Detail-M4b — change `[defmacro name [params] body]` to `[defmacro name fn]`; change `Expr::DefMacro { name, params, body }` to `Expr::DefMacro { name, transformer }` (`doc/feature/macros-cluster.md`)
- [ ] `doc/feature/macros-cluster.md` §Phase-M2a — add `stdlib/formatter/pretty.llt` alongside `compact.llt`; note that the `--tinct-fmt` flag enables the tinct-hosted path (`doc/feature/macros-cluster.md`)
- [ ] `doc/feature/macros-cluster.md` §Phase-M3b and §Sprint-Summary M3 row — rename `stdlib/formatter/format.llt` to `stdlib/formatter/pretty.llt`; mark `formatter-full` (as `pretty.llt`) as complete (`doc/feature/macros-cluster.md`)
- [ ] `doc/feature/macros-cluster.md` §Cross-Cutting-Concerns — add a note that current stdlib macro registration uses `STDLIB_MACROS` + `register_stdlib_macros`; `stdlib-defmacro` sprint will replace this with dynamic `[defmacro ...]` declarations (`doc/feature/macros-cluster.md`)
- [ ] `doc/feature/io.md` §The Stdlib Layer — update `write-file` description to reflect actual implementation: calls `write cap path content` (builtin `write` with DirCap directly); `write-file-atomic` calls `write-atomic cap path content` (`doc/feature/io.md`)
- [ ] `doc/feature/lib-supplemental.md` §Extended String Utilities — update `pad-left` and `pad-right` to the 3-parameter signature `[s width pad-char]` as implemented in `stdlib/strings.llt` (`doc/feature/lib-supplemental.md`)
- [ ] `doc/feature/lib-supplemental.md` §Extended String Utilities — remove `str-contains?` (it's a Rust builtin, not in `strings.llt`); remove `str-count`, `str-take`, `str-drop` from the pure-tinct `strings.llt` table or mark them as not-yet-implemented (`doc/feature/lib-supplemental.md`)
- [ ] `doc/feature/lib-supplemental.md` §Bytes Type and §Bitwise Primitives — note that `base64-encode` / `hex-decode` currently operate on `String` because `Value::Bytes` is not yet implemented; update accordingly (`doc/feature/lib-supplemental.md`)
- [ ] `doc/feature/lib-supplemental.md` §Phase 1 — clarify that `between`, `non-negative`, and `positive` are defined in `stdlib/prelude.llt`; the named width type aliases (`UInt8`, etc.) and `to-bytes` are in `stdlib/numeric.llt` (opt-in via include) (`doc/feature/lib-supplemental.md`)
- [ ] `doc/feature/lib-supplemental.md` §Extended String Utilities — move `str-repeat` and `str-find` from the `strings.llt` table to the prelude/builtin section with a note that they are always available without include (`doc/feature/lib-supplemental.md`)
- [ ] `doc/feature/lib-datetime.md` §`stdlib/datetime.llt` — update `days-between` semantics to reflect actual code behavior (positive when second arg is later due to `timestamp-diff b a`), or update `stdlib/datetime.llt` to match feature doc intent (`timestamp-diff a b`, positive when first arg is later); feature doc wins — fix the code (`doc/feature/lib-datetime.md`, `stdlib/datetime.llt`)
- [ ] `doc/feature/lib-datetime.md` §`stdlib/datetime.llt` — update `timestamp-in-range?` to match feature doc parameter order `[start end t]`; fix `stdlib/datetime.llt` accordingly: `[fn [start@Timestamp end@Timestamp t@Timestamp] ...]` (`doc/feature/lib-datetime.md`, `stdlib/datetime.llt`)
- [ ] `doc/feature/lib-regex.md` — add an "Implementation Status" note at the top indicating the current `stdlib/regex.llt` is a literal-matching MVP; the Thompson NFA design is the spec for a future sprint (`doc/feature/lib-regex.md`)
- [ ] `doc/feature/lib-tls.md` §stdlib and §Type Checker — update `fetch` signature to match `net.llt`: takes `NetCap` and a `String` URL (not a parsed `Url` object); `http-get` takes 2 params not 4 (no `headers` or `tls-opts`) (`doc/feature/lib-tls.md`)
- [ ] `doc/feature/lib-net-v2.md` §Protocol Library — update `socks5-layer` description: `protocols/socks5.llt` provides wire-format message builders (`build-socks5-greeting`, `build-socks5-connect`, `parse-socks5-response`), not a complete `socks5-layer` function; `socks5-layer` wrapper is not yet implemented (`doc/feature/lib-net-v2.md`)
- [ ] `doc/feature/lib-net-v2.md` §Layer Protocol — remove `http-connect-layer` from the list or note it as not yet implemented (absent from `stdlib/net.llt`) (`doc/feature/lib-net-v2.md`)
- [ ] `doc/feature/lib-net-v2.md` §`fetch` function — update the 3-param signature to the actual 2-param implementation in `net.llt` (`doc/feature/lib-net-v2.md`)
- [ ] `doc/feature/templating.md` — replace all occurrences of `tinct eval` with `tinct run` (the actual CLI command) (`doc/feature/templating.md`)
- [ ] `doc/feature/templating.md` §Standard Formatters — update pipeline examples to use `[include libdir "out/yaml.llt"]` instead of a direct path argument like `stdlib/out/yaml.llt` (`doc/feature/templating.md`)
- [ ] `doc/feature/numeric-types.md` §Phase 1 — add `to-bytes` to the description of `stdlib/numeric.llt` as a numeric utility function (currently undocumented in the feature doc) (`doc/feature/numeric-types.md`)
- [ ] `doc/feature/typeclasses.md` §Phase 2 — add a cross-reference to `doc/feature/hkt-monads.md` noting that the concrete typeclass hierarchy (Functor/Applicative/Monad/Foldable/Traversable) was specified as part of HKT and that Phase 2 descriptions here are superseded by that design (`doc/feature/typeclasses.md`)
- [ ] `doc/feature/nominal-variants.md` §Overview — add a note that nominal variants are not merely an alternative to structural ADTs under BAS but are required: S-RcdTop collapses disjoint-key structural unions to `Top`, making nominal variants the only viable discriminated union mechanism under the BAS type system (`doc/feature/nominal-variants.md`)
- [ ] `doc/feature/algebraic-data-types.md`, `doc/feature/union-types.md`, `doc/feature/parameterized-dict.md` — add cross-references to `doc/feature/boolean-algebraic-subtyping.md` clarifying that the `@Record`/`@Dict` typing and open-record semantics described in those docs were superseded by BAS (closed records, no RowVar tails, `@Dict` = closed empty record not `Record ∨ Map` union) (`doc/feature/algebraic-data-types.md`, `doc/feature/union-types.md`, `doc/feature/parameterized-dict.md`)

### fdv-main-doc-updates: Correct stale content in main doc/*.md files

For findings where the feature doc or code is the source of truth and the main doc is wrong.

- [ ] `doc/12-tooling.md` line 7 — change the cross-reference from `doc/whatif/completed/parser-rewrite.md` to `doc/feature/parser-rewrite.md` (the canonical post-implementation document) (`doc/12-tooling.md`)
- [ ] `doc/02-syntax.md` §5 Document Separator Grammar and §6 Complete Grammar — document the full section header syntax: `--- %name@Type expects: Type`; add grammar rules for each optional component (`%name`, `@Type`, `expects:` pragma); reference `doc/09-documents.md` for semantics (`doc/02-syntax.md`)
- [ ] `doc/02-syntax.md` §3.1 File, Document, and Expression — update the `doc_separator` rule to reference the extended header grammar including `%name`, `@Type`, and `expects:` (`doc/02-syntax.md`)
- [ ] `doc/03-data-model.md` §Part 5 — change `check_dot_access_int` in the type rules table to `check_dot_access (DotKey::Int arm)` to accurately reflect the implementation (`doc/03-data-model.md`)
- [ ] `doc/03-data-model.md` §Part 5 — remove the Remy-style unification description and replace with the BAS description: dot access on an open record returns `Any`; closed records return the declared field type or error; align with `doc/05-type-annotations.md §Row polymorphism syntax (removed under BAS)` (`doc/03-data-model.md`)
- [ ] `doc/04-functions.md` §Function Definition formal grammar — change `fn_form = { keyword_fn ~ fn_annotation? ~ param_list ~ value }` to `value+` (or add a prose note that multiple body expressions are wrapped in `Expr::Sequential`) (`doc/04-functions.md`)
- [ ] `doc/08-evaluation.md` §Laziness Design — add a section documenting `Expr::Sequential` inside fn bodies: lazy intermediate bindings, environment extension, result = last expression; note that the CEK machine routes `Sequential` to `eval_recursive` via the `eval_materialize.rs` fallback (`doc/08-evaluation.md`)
- [ ] `doc/04-functions.md` §Special Forms vs Stdlib Functions table — add `match` as a fourth language-level special form; add `quote`, `unquote`, `unquote-splice` (note that `unquote`/`unquote-splice` are only valid inside `[quote ...]`) (`doc/04-functions.md`)
- [ ] `doc/11-stdlib.md` §Language Builtins (Special Forms) list — add `match`, `quote`, `unquote`, `unquote-splice` (`doc/11-stdlib.md`)
- [ ] Add a `doc/14-patterns.md` section (or `doc/13-match.md`) documenting `[match]` syntax, arm patterns, exhaustiveness behavior (runs only for `Type::Union` scrutinees), and dynamic `MatchError` on no-arm-match (`doc/14-patterns.md`)
- [ ] `doc/11a-builtins.md` §Type Introspection — update `record?` and `map?` row descriptions: both currently return true for any Dict/Overlay value; key-type distinction is type-level only (runtime has no key-type tracking) (`doc/11a-builtins.md`)
- [ ] `doc/11-stdlib.md` §Type Introspection builtin list (~line 202) and Type Predicates section table (~line 530) — add `record?` and `map?` as Rust builtins with accurate runtime behavior descriptions (`doc/11-stdlib.md`)
- [ ] `doc/11a-builtins.md` §Evaluation Control — update `eval-ast` row: replace "requires AST representation in Value" with "takes a Dict in AST schema format (as produced by `[quote ...]`); converts via `dict_to_ast` and evaluates in the current environment" (`doc/11a-builtins.md`)
- [ ] `doc/15-ast.md` §AST Dict Schema — change `doc/whatif/ast-schema.md` to `doc/feature/ast-schema.md` (the file was moved) (`doc/15-ast.md`)
- [ ] `doc/06-type-inference.md` §Let-Generalization TypeScheme block — add `constraints: Vec<Constraint>` and `label_vars: Vec<String>` fields; update the TypeScheme grammar `σ ::= ∀(α₁...αₙ, ρ₁...ρₘ). τ` to remove row variable portion; replace with `σ ::= ∀(α₁...αₙ). [C₁ a₁, ...] τ` (type_vars + constraints + body) (`doc/06-type-inference.md`)
- [ ] `doc/06-type-inference.md` lines 828 and 967 — replace all occurrences of `key@"l"` / `key@"k"` Label annotation syntax with `key@Label` (anonymous) and `key@[label: l]` (named), per the `label-annotation-syntax` sprint (`doc/06-type-inference.md`)
- [ ] `doc/06-type-inference.md` §`[do]` Inference — add an implementation status note: the inferred `[do]` form is not yet implemented (requires `hkt-do-macro` sprint); the explicit `[do monad steps...]` form is implemented (`doc/06-type-inference.md`)
- [ ] `doc/06-type-inference.md` §Generic Functions — add a note that `sequence` and `traverse` are specified but not yet in `stdlib/prelude.llt`; they require the `hkt-stdlib` sprint (`doc/06-type-inference.md`)
- [ ] `doc/06-type-inference.md` §Higher-Kinded Types table — add an implementation note that `Mappable` is currently registered as a placeholder `Kind::Type` class and will be promoted to `Kind::Operator` in the `hkt-mappable-appendable` sprint (`doc/06-type-inference.md`)
- [ ] `doc/07-type-extensions.md` BAS algebra annotation table — add the `α → @a` row and the `μα.A → @[AliasName ...]` row (`doc/07-type-extensions.md`)
- [ ] `doc/07-type-extensions.md` line 58 — add cross-reference to `doc/feature/boolean-algebraic-subtyping.md` alongside (or instead of) `doc/whatif/completed/boolean-algebraic-subtyping.md` (`doc/07-type-extensions.md`)
- [ ] `doc/06-type-inference.md` §Type Grammar — replace `Record(f₁:τ₁...fₙ:τₙ, ρ)` / `ρ ::= Closed` with `Record(f₁:τ₁...fₙ:τₙ)` (no row-rest parameter; closedness is the only mode under BAS); add a one-line note that the `ρ` notation is archived (`doc/06-type-inference.md`)
- [ ] `doc/10-errors.md` §`try` prose, code example, and §Part 6 formal rules — replace structural dict return (`[ok: value]` / `[err: message]`) with nominal variant form: `[Ok value]` / `[Err message]`; update `[TRY]` / `[TRY-ERR]` rules to use `Variant("Ok", θ(v))` / `Variant("Err", θ(msg))` (`doc/10-errors.md`)
- [ ] `doc/10-errors.md` line 348 — replace "`error` has type `Str → Any` — tinct has no bottom type" with "`error` has type `Str → Never` — `error` never returns; `Never` is the bottom type" (`doc/10-errors.md`)
- [ ] `doc/07-type-extensions.md` §`Record` / `Dict` note — add an implementation note that `@Dict` currently resolves as `Record(Row{})` (width-subtyping fallback); the full `Dict = Record ∨ Map` union semantics are a target state for when BAS constraint resolution is fully implemented (`doc/07-type-extensions.md`)
- [ ] `doc/05-type-annotations.md` line 186 — change "`@Null` resolves to `Type::Record(Row::Empty)`" to "`@Null` resolves to `Type::Record` with no fields (the closed empty-record type)" (`doc/05-type-annotations.md`)
- [ ] `doc/07-type-extensions.md` §%@Type / pipeline annotations — add a brief `%@Type Pipeline Annotations` subsection cross-referencing `doc/feature/structural-contracts.md`; the type checker implements `%` binding at `src/typecheck.rs:470-482` but this syntax is undocumented in the main type docs (`doc/07-type-extensions.md`)
- [ ] `doc/12-tooling.md` §Tinct-Hosted-Formatter — clarify that `pretty.llt` IS the full formatter; remove or rewrite the sentence describing a separate `format.llt` as the "full" formatter (`doc/12-tooling.md`)
- [ ] `doc/12-tooling.md` §Compact-Formatter-Modes — add a note that `--tinct-fmt` opts into the tinct-hosted formatter; without this flag, compact modes use the Rust formatter (`doc/12-tooling.md`)
- [ ] `doc/09-documents.md` §Pure Language — remove the claim that there is no `$write`/`$stdin`/etc.; update to describe the capability-based I/O model briefly, referencing `doc/feature/io.md` (`doc/09-documents.md`)
- [ ] `doc/09-documents.md` §Include Mechanism examples (~line 586) — use cap-qualified form `[include libdir "lib/utils.llt"]`; update the RESOLVE rule in §Part 2: Path Resolution to reflect DirCap-based resolution rather than `base_dir` path resolution (`doc/09-documents.md`)
- [ ] `doc/11-stdlib.md` optional module table (~line 267) — update `datetime.llt` entry to list actual llt-provided functions: `days-between`, `timestamp-in-range?`, `format-date` (once added); remove the Rust builtin names (`doc/11-stdlib.md`)
- [ ] `doc/11-stdlib.md` §Standard Formatters table (~line 607) — change `fmt/yaml.llt`, `fmt/json.llt` etc. to `out/yaml.llt`, `out/json.llt` etc. matching the actual `stdlib/out/` directory layout; remove all `fmt/` path references (`doc/11-stdlib.md`)
- [ ] `doc/11-stdlib.md` optional module table (~line 268) — update `regex.llt` entry: change function names to `re-compile`, `re-match`, `re-find`, `re-findall`, `re-replace`, `re-split` (not `regex-match`, `regex-find-all`, `regex-replace`) (`doc/11-stdlib.md`)
- [ ] `doc/11-stdlib.md` — remove the claim that `uri-params`, `uri-origin`, `uri->string` are Rust builtins always available; change the optional module table entry for `net.llt` to list these as llt-implemented functions requiring `[include libdir "net.llt"]` (`doc/11-stdlib.md`)
- [ ] `doc/11-stdlib.md` (~line 316) — note that `socks5-connect` and `proxy-connect` are stub builtins that return "not yet implemented" errors and are tracked for removal in the `net-gaps` sprint; update `doc/feature/lib-tls.md` §Network Stack Summary to note that `socks5-layer` (pure tinct in `protocols/socks5.llt`) replaces them (`doc/11-stdlib.md`, `doc/feature/lib-tls.md`)

### fdv-code-gaps: Implement missing functionality

For findings where the feature doc says something should work but the code doesn't have it yet — genuine gaps, not just doc issues.

- [ ] [**S-RcdTop/ADT design tension**] `doc/feature/union-types.md` and `doc/feature/algebraic-data-types.md` present structural discriminated unions like `{ok: T} | {err: S}` as working ADTs, but under BAS S-RcdTop (`src/types.rs:882`) collapses disjoint-field record unions to `Type::Top`. This is not a doc fix — it is a genuine type-system design tension. Nominal variants (`Value::Variant`, `Pattern::Constructor`) are the current workaround, but `Type::NominalVariant` does not yet exist. Track as: (1) update both feature docs with a prominent §Design Tension section explaining S-RcdTop collapse and the nominal-variant workaround; (2) add `Type::NominalVariant { tag: String, payload: Option<Box<Type>> }` to `src/types.rs` with `is_subtype` rules per `doc/feature/nominal-variants.md`; (3) wire `Pattern::Constructor` arms in `infer_match` to use `Type::NominalVariant` (`src/types.rs`, `src/typecheck.rs`, `doc/feature/union-types.md`, `doc/feature/algebraic-data-types.md`)
- [ ] `stdlib/numeric.llt` — add `UInt64@[doc: "Unsigned 64-bit integer (>= 0)"]: [type Int@[is: [>= _ 0]]]` and `Int64@[doc: "64-bit signed integer (alias for Int)"]: [type Int]`, matching the feature doc's eight named width types (`stdlib/numeric.llt`)
- [ ] `stdlib/encoding.llt` — replace the pure-tinct `xor`/`xor-impl`/`xor-step`/`xor-bit`/`pow2`/`pow2-impl` helpers with the `bxor` Rust builtin; update `mask-apply-step` to call `[bxor data-code mask-code]` directly (the pure-tinct implementation is limited to 8-bit values) (`stdlib/encoding.llt`)
- [ ] `stdlib/io.llt` — add `append-file`, `open-write`, `open-append`, and `write-lines` per `doc/feature/io.md` §The Stdlib Layer and §Streaming File I/O (`stdlib/io.llt`)
- [ ] `stdlib/datetime.llt` — add `format-date` using the 3-arg `pad-left` signature (requires `[include libdir "strings.llt"]`): `[pad-left [str [timestamp-month t]] 2 "0"]` per `doc/feature/lib-datetime.md` (`stdlib/datetime.llt`)
- [ ] `src/type_env.rs` — add `record?` and `map?` to `TypeEnv::with_builtins()` with type signatures `Unknown → Bool` (conservative fallback until narrowing is implemented); both builtins are registered in `standard_builtins()` but invisible to the type checker (`src/type_env.rs`)
- [ ] `src/type_env.rs` — implement precise `get?` type inference: when the dict arg is `Map[K V]`, return `Union([V, Record(Row{})])` (V | Null); when the dict arg is a `Record` with a known field, return `Union([field_type, Record(Row{})])`. The current registration is `(Unknown, Unknown) → Unknown` and the claimed type-checker special-casing is not implemented (`src/type_env.rs`, `src/typecheck.rs`)
- [ ] `doc/feature/null-semantics.md` — track the `null` keyword sprint (desugaring `null` → `[]`) or explicitly mark it as rejected; until a decision is made, update the feature doc to note the keyword is unscheduled; add a TODO item or decision note (`doc/feature/null-semantics.md`, `TODO.md`)

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

### typecheck-gaps: Small typecheck correctness fixes

Deferred typecheck correctness items found in source comments.

- [ ] Union narrowing in `collect_pattern_bindings` (`src/typecheck.rs:1233`): when the match scrutinee is `Type::Union`, intersect field types across all Record members instead of falling through to `Unknown`; a field present in every member with consistent type should bind to that type in the pattern body (`src/typecheck.rs`)
- [ ] Propagate recursion depth guard in `resolve_type_expr_with_guard` (`src/typecheck_annot.rs:1219`) through the `_` fallback arm — currently delegates to the non-guard version, meaning structural recursion through non-VarRef positions (e.g. dict-body type aliases) is not guarded; propagate the guard into all sub-expressions (`src/typecheck_annot.rs`)
- [ ] Same fix for `resolve_type_dict_with_guard` (`src/typecheck_annot.rs:1239`): pass the recursion guard through field type resolution for recursive structural type aliases (`src/typecheck_annot.rs`)
- [ ] Add `Type::types_are_disjoint(t1: &Type, t2: &Type) -> bool` to `src/types.rs`: `(Never, _)` → true; `(Any|Unknown, _)` → false (conservative); different concrete primitive pairs (`Int`/`String`, `Int`/`Bool`, `Int`/`Float`, `String`/`Bool`, etc.) → true; `Record` vs any primitive → true; `(Union(ms), t)` → `ms.iter().all(|m| disjoint(m, t))`; `(Intersection(ms), t)` → `ms.iter().any(|m| disjoint(m, t))`; anything else → false (conservative) (`src/types.rs`)
- [ ] Replace the `(_, Type::Negation(a)) => true` placeholder at `src/types.rs:353` with `(sub_ty, Type::Negation(a)) => Type::types_are_disjoint(sub_ty, a)` — `T <: ~A` holds iff T and A are disjoint; the existing `(Negation(t1), Negation(t2)) => is_subtype(t2, t1)` contravariant arm at line 347 is correct and unchanged (`src/types.rs`)
- [ ] Tests: `Int <: ~String` (disjoint primitives → holds); `Int <: ~Int` (same type → fails); `String | Int <: ~Bool` (both members disjoint from Bool → holds); `[@[[without Bool]] 42]` TypeAssert passes; `[@[[without Int]] 42]` TypeAssert fails at runtime; union match binding uses field type from Record members; recursive dict type alias does not stack-overflow (`tests/corpus/eval/typecheck/`)

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

### type-warning-channel: Add three-tier diagnostic system to the type checker

The type checker currently returns only `Vec<TypeError>` — all diagnostics are fatal. This sprint adds a three-tier notification system: `Info` (hint/suggestion), `Warn` (concern), `Err` (fatal). `--strict` bumps every diagnostic up one level: Info→Warn, Warn→Err. This maps directly onto LSP severity levels and gives a principled model for all future type quality diagnostics.

- [ ] Add `TypeDiagnostic` type to `src/error.rs` with a `Level` enum `{ Info, Warn, Err }`; include `message: String`, `span: Span`, `code: &'static str`, `level: Level`; `--strict` bump is applied at emission time by a `bump(level) -> Level` function that shifts Info→Warn, Warn→Err, Err→Err (`src/error.rs`)
- [ ] Update `typecheck_file` to return `(Rc<TypeEnv>, Vec<TypeError>, Vec<TypeDiagnostic>)` — existing `TypeError` vec for hard errors, new `TypeDiagnostic` vec for the three-tier system; update `typecheck_file_with_types` to match (`src/typecheck.rs`)
- [ ] Update all call sites in `src/lib.rs`, `src/main.rs`, `src/lsp/document.rs` to destructure the new return and route diagnostics by level (`src/lib.rs`, `src/main.rs`, `src/lsp/document.rs`)
- [ ] CLI output: `Info` → dimmed hint text; `Warn` → yellow warning; `Err` (from strict bump) → red error, fatal; exit 0 when only Info/Warn present, non-zero on Err (`src/main.rs`)
- [ ] LSP output: `Info` → `DiagnosticSeverity::Hint` or `Information`; `Warn` → `DiagnosticSeverity::Warning`; `Err` → `DiagnosticSeverity::Error` (`src/lsp/document.rs`)
- [ ] Tests: file with Info/Warn diagnostics compiles and exits 0; `--strict` escalates Warn to Err; LSP emits correct severity per level (`tests/corpus/eval/`, `tests/lsp_corpus_tests.rs`)

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

See `doc/whatif/dir-cap-permissions.md` (Accepted 2026-05-11). Extends `--cap-fs` (and `--cap-file`) with an optional `:MODE` suffix using letter bundles and an extended `:[Cap1 Cap2 ...]` list syntax; adds a `DirPerms` bitfield to `Value::DirCap`; enforces permissions in DirCap-consuming builtins; exposes a row-polymorphic `DirCap[Writable ...]` type. No mode on either flag = full access (all capabilities). **Spec chapters:** `doc/whatif/dir-cap-permissions.md`.

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
