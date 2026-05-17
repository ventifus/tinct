# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Type Stage Features

*All sprints below depend on `type-stage-infra`.*

### chr-module-split: Extract type_def.rs, type_infer.rs, type_normalize.rs, type_unify.rs

See `doc/whatif/chr-unification.md §Module Restructuring`. **Spec chapters:** `doc/06-type-inference.md §Multi-Parameter Type Classes`. Breaks the `types.rs ↔ value.rs` circular dependency that would arise from `NormCtxt` carrying `Rc<Environment>`. All existing `use crate::types::...` call sites are unchanged via re-exports.

- [ ] Create `src/type_def.rs` — move `Type` enum, `Row`, `RowTail`, `TypeKey`, and purely structural methods (`collect_type_vars`, `has_type_vars`, `occurs_in`, `display helpers`) from `src/types.rs`; update `src/value.rs` to `use crate::type_def::Type` instead of `types::Type` (`src/type_def.rs`, `src/value.rs`, `src/types.rs`)
- [ ] Create `src/type_class.rs` — move `ClassDecl`, `Constraint`, `ClassEnv`, `InstanceEnv` and their impls from `src/types.rs`; update all import sites (`src/type_class.rs`, `src/typecheck.rs`, `src/type_unify.rs`, `src/type_env.rs`, `src/imports.rs`)
- [ ] Create `src/type_infer.rs` (top-level module, not submodule) — move `InferState`, `Substitution`, `Levels` and their impls from `src/types.rs`; update all import sites (`src/type_infer.rs`, `src/typecheck.rs`, `src/typecheck_dict.rs`, `src/type_unify.rs`)
- [ ] Create `src/type_normalize.rs` — placeholder module with empty `NormCtxt` struct and stub `normalize()` signature (no impl yet); move `normalize_union()`, `normalize_intersection()`, and `Type::Display` here (`src/type_normalize.rs`, `src/types.rs`)
- [ ] Promote `src/type_unify.rs` to top-level module — remove `#[path = "type_unify.rs"]` from `src/types.rs`; add `mod type_unify;` to `src/lib.rs`; verify import chain compiles (`src/type_unify.rs`, `src/lib.rs`, `src/types.rs`)
- [ ] Update `src/types.rs` to be a thin façade: `pub use type_def::*; pub use type_class::*; pub use type_infer::*; pub use type_normalize::*;` — all existing `use crate::types::...` call sites continue working without changes (`src/types.rs`)
- [ ] Tests: `cargo build` and full test suite pass with no functional changes (`tests/`)

**Depends on:** `type-stage-infra`

### chr-normalization: TypeStageApp, NormCtxt, normalize(), deferred equality

See `doc/whatif/chr-unification.md §Normalization`, `§TypeStageApp`, `§FD Elaboration`. **Spec chapters:** `doc/06-type-inference.md §Multi-Parameter Type Classes`, `§TypeStageApp unification rules`.

- [ ] Add `Type::TypeStageApp { fn_name: String, args: Vec<Type> }` to `src/type_def.rs`; add `unreachable!("TypeStageApp not yet handled")` stub arms to every exhaustive match in `src/types.rs`, `src/type_class.rs`, `src/type_infer.rs` — compile-error-driven discovery of all sites (`src/type_def.rs`, `src/types.rs`, `src/type_class.rs`, `src/type_infer.rs`)
- [ ] Fill TypeStageApp match arms in `src/type_unify.rs` and `src/type_env.rs`: add `TypeStageApp` to `collect_type_vars`, `has_type_vars`, `occurs_in` (recurse into `args`); add display form `FnName(arg1, arg2)` in `Type::Display`; update `type_key()` to return a stable key for TypeStageApp (`src/type_unify.rs`, `src/type_env.rs`, `src/type_normalize.rs`)
- [ ] Fill TypeStageApp match arms in `src/typecheck.rs`, `src/typecheck_annot.rs`, `src/typecheck_dict.rs`: default to `normalize(TypeStageApp(...), ctx)` in inference rules; treat as concrete for is_subtype/is_consistent (`src/typecheck.rs`, `src/typecheck_annot.rs`, `src/typecheck_dict.rs`)
- [ ] Implement `type_to_dict` (with literal widening: `IntLiteral→Int`, etc.) and `dict_to_type` (with `kind: "multi-output"` sentinel support) in `src/type_normalize.rs` (`src/type_normalize.rs`)
- [ ] Implement `NormCtxt` in `src/type_normalize.rs`: fields `subst`, `type_stage_env: Rc<Environment>`, `alias_env`, `class_env: &ClassEnv`, `depth: usize`, `max_depth: usize = 256`, `call_stack: Vec<String>`, normalization cache `HashMap<(String, Vec<TypeKey>), Type>`; add `NormCtxt::from(subst, state)` and `NormCtxt::minimal()` constructors (`src/type_normalize.rs`)
- [ ] Implement `normalize(ty: Type, ctx: &NormCtxt) -> Type`: step 1 substitution apply; step 2 TypeStageApp (Unknown-in-args→Unknown, call_stack cycle check, is_ground check, type_to_dict + eval + dict_to_type + recurse); step 3 BAS simplification; step 4 literal widening; step 5 alias expansion via `normalize(expand, ctx)`; step 6 recursive child normalization (`src/type_normalize.rs`)
- [ ] Thread `NormCtxt` into `InferState`: add `norm_ctxt: NormCtxt<'_>` construction at the start of `unify()`; update `unify()` to call `normalize(a, &norm)` and `normalize(b, &norm)` before `unify_normalized()` (`src/type_infer.rs`, `src/type_unify.rs`)
- [ ] Implement 5 `TypeStageApp` cases in `unify_normalized()`: CONGRUENCE (injective: pairwise arg unify), DEFER (non-injective: push to `deferred_equalities`), APART (different fn: TypeError), STUCK (vs ConcreteType non-TypeVar: TypeError with message), VAR (bind TypeVar, occurs check traverses args) (`src/type_unify.rs`)
- [ ] Add `deferred_equalities: Vec<(Type, Type)>` to `InferState`; implement processing loop at end of `unify()`: normalize both sides; if both concrete, call `unify(lhs', rhs')`; discard entries containing only generalized TypeVars at let-generalization time; add `ClassDecl.resolver_injective: bool` field (defaults `false`) (`src/type_infer.rs`, `src/type_class.rs`, `src/type_unify.rs`)
- [ ] Update `improve_functional_dependency`: look up `class_decl.resolver` in type-stage Env and call via `eval()` → `dict_to_type`; retain `lookup_arithmetic_instance` fast path for built-in classes; extend BAS deferral predicate to include `Type::Unknown` and `Type::TypeVar`; add Unknown-in-determining-positions → return `Type::Unknown` rule (`src/type_unify.rs`)
- [ ] Add `NormCtxt` parameter to `entails()`; pass `NormCtxt::minimal()` at declaration-time call sites where type-stage Env may be absent (`src/type_unify.rs`)
- [ ] Tests: `[fn [x y] [+ x y]]` infers `Add a b c => Fn@c [a b]`; `[fn [x@Int y@Float] [+ x y]]` infers `Float`; `[= [+ 1 2.0] [+ 1.5 2.5]]` passes without false error; depth-limit TypeError names the resolver; `TypeStageApp` appears in error display as `AddResult(a, b)`; corpus tests in `tests/corpus/eval/typecheck/` (`tests/`)

**Depends on:** `chr-module-split`

### chr-class-instance: [class ...] two-bracket form and [instance ...] match-arm syntax

See `doc/whatif/chr-unification.md §Class Body Structure`, `§Instance Syntax`, `§Expr::ClassDecl`, `§Expr::InstanceDecl`. **Spec chapters:** `doc/06-type-inference.md §Typeclass Declarations and Instances`.

- [ ] Extend `Expr::ClassDecl` in `src/ast.rs`: add `determines: Vec<Spanned<Expr>>`, `resolver: Option<Spanned<Expr>>`; change `superclasses: Vec<(String, String)>` → `Vec<(String, Vec<String>)>`; add `unreachable!()` stub arms in `src/eval.rs`, `src/formatter.rs`, `src/desugar.rs`, `src/resolve.rs`, `src/lsp/analysis.rs`, `src/ast_dict.rs` (compile-error-driven) (`src/ast.rs`, ~6 files)
- [ ] Update parser for `[class ...]` two-bracket form: add `structural_metadata: Option<Spanned<Expr>>` to `StackFrame::ClassDecl`; route second positional `Expr::Dict` to `structural_metadata` in `push_expr_to_parent` (currently hard-errors); extract `determines:`, `resolver:`, `kinds:`, `superclasses:` from `structural_metadata.entries` in `CloseBracket` ClassDecl handler (`src/parser.rs`)
- [ ] Redesign `Expr::InstanceDecl` AST: change `{ class_name, instance_type, methods }` → `{ class_name, arms: Vec<(Spanned<Expr>, Vec<Spanned<Entry>>)> }`; add stub arms to `src/eval.rs`, `src/formatter.rs`, `src/desugar.rs`, `src/resolve.rs`, `src/lsp/analysis.rs`, `src/ast_dict.rs` (`src/ast.rs`, ~6 files)
- [ ] Update parser for `[instance ...]` match-arm form: redesign `StackFrame::InstanceDecl` with `pending_arm_key: Option<Spanned<Expr>>`, `span_start`; add `VarRef` branch in `push_expr_to_parent` InstanceDecl arm for bare class name; method dicts arrive as completed `Expr::Dict` nodes via `push_value` (bracket-form required); update `InstanceDecl` Display impl (`src/parser.rs`, `src/ast.rs`)
- [ ] Add `Expr::PatternDecl { bindings: Vec<Spanned<Expr>> }` to `src/ast.rs`; add `StackFrame::PatternDecl` to `src/parser.rs`: dispatch `pattern` keyword with colon-ahead rejection rule (`!matches!(peek_next_horizontal(...), Colon)`); inner bracket uses iterative Dict frame producing `Expr::Annotated` nodes converted to bindings by `push_expr_to_parent` (`src/ast.rs`, `src/parser.rs`)
- [ ] Update `Expr::ClassDecl` typecheck handler: validate `determines:` entries (2-element lists, known param names); resolve param names to positional indices; validate coverage and consistency conditions with error templates naming conflicting arms; retire `f@Operator` — annotation resolver routes `kinds:` in `fn@[...]` and class structural brackets to `kind_env`; add migration error for `f@Operator` outside these positions (`src/typecheck.rs`, `src/typecheck_annot.rs`)
- [ ] Update `Expr::InstanceDecl` typecheck handler: validate arm param count matches class; run disjointness/coverage/consistency as a batch (after all `[instance ...]` for the class are processed and type-stage Env populated); compute and set `ClassDecl.resolver_injective`; register arms in scope-local InstanceEnv; typecheck each arm's method implementations against class method signatures (`src/typecheck.rs`)
- [ ] Tests: `[class [a b c] [determines: [[[a b] c]] resolver: AddResult] +: [fn@c [a b]]]` parses and typechecks; `[instance ...]` with `[pattern [a@Int b@Int c@Int]]:` arm registered correctly; disjointness error names conflicting arms with spans; coverage error names offending variable; `kinds:` in class structural bracket routes correctly; corpus tests in `tests/corpus/eval/typecheck/` (`tests/`)

**Depends on:** `chr-normalization`

### chr-prelude: Arithmetic class migration and boundary guard elaboration

See `doc/whatif/chr-unification.md §Prelude Class Declarations`, `§Post-inference boundary guard elaboration pass`. **Spec chapters:** `doc/06-type-inference.md §Multi-Parameter Type Classes`, `doc/feature/gradual-typing.md §Evaluator`.

- [ ] Write `--- stage: type` resolver functions (`AddResult`, `SubResult`, `MulResult`, `DivResult`) in `stdlib/prelude.llt`; declare `Addable`, `Subtractable`, `Multipliable`, `Divisible` classes with two-bracket form; declare instances using `[pattern ...]` match-arm syntax (4 arms each: Int+Int, Float+Float, Int+Float, Float+Int) (`stdlib/prelude.llt`)
- [ ] Remove pre-registered arithmetic class/instance Rust declarations in `src/type_env.rs` superseded by prelude; retain `lookup_arithmetic_instance` in `src/type_unify.rs` as O(1) fast path (built-in class check → table; user-defined classes → resolver eval path); keep builtin type signatures for `+`, `-`, `*`, `/` as `Add a b c => a → b → c` etc. (`src/type_env.rs`, `src/type_unify.rs`)
- [ ] Implement `elaborate_boundary_guards` in `src/typecheck.rs` (or `src/typecheck_elaborate.rs`): post-inference pass over the type map; for each expression where inferred type is `Unknown` and contextual expected type is concrete, call `normalize(τ_ctx, NormCtxt::final(...))`, emit `TypeError` if not `is_concrete`, otherwise write normalized expected type to the expression's guard annotation `RefCell` field; handle `---` pipeline crossings with `Positive` polarity (`src/typecheck.rs`)
- [ ] Update `src/eval.rs` to read guard annotations written by `elaborate_boundary_guards`: when `eval()` processes an expression with a guard annotation, wrap its result thunk in `ThunkState::Guarded(inner, expected_concrete, BlameLabel)`; nested guard collapsing — if inner is already `Guarded`, use `inner.inner` as the actual inner thunk (`src/eval.rs`)
- [ ] Tests: `[+ 1 2]` infers `Int`; `[+ 1 2.0]` infers `Float`; `[fn [x y] [+ x y]]` infers `Add a b c => Fn@c [a b]`; user-defined `[class [a b c] [determines: [[[a b] c]] resolver: MyResult]]` with `[instance ...]` arms participates correctly; `[= [+ 1 2.0] [+ 1.5 2.5]]` passes; boundary guard fires on `Unknown → Int` boundary producing blamed error; corpus tests in `tests/corpus/eval/typecheck/` and `tests/corpus/eval/stdlib/` (`tests/`)

**Depends on:** `chr-class-instance`

### isorecursive-types: μ-types and coinductive subtype checking

See `doc/whatif/isorecursive-types.md`. **State: Proposal** — design not yet approved; sprint tasks to be written after /rnd approval. `type-stage-infra` is the required groundwork (`mu`/`recvar` combinators live in the `--- stage: type` section; `dict_to_type()` will need `kind: "recursive"` and `kind: "recvar"` arms).

**Depends on:** `type-stage-infra`

### validate-tinct-rewrite: Rewrite validate's recursive schema walk in tinct

`validate_value` in `src/builtins_meta.rs` (~267 lines) is the largest remaining Rust function that could be expressed in tinct. `regex-match?` is now available. Full rewrite of the recursive schema walk (the `fields:` and `items:` recursion) requires recursive dict schema support to type the schema dict.

**Depends on:** `isorecursive-types`

- [ ] Define the schema dict type in `stdlib/prelude.llt` using a recursive type alias (`mu`-type); covers all schema keys: `type`, `min`, `max`, `min-length`, `max-length`, `pattern`, `required`, `default`, `items`, `fields`, `enum` (`stdlib/prelude.llt`)
- [ ] Rewrite `validate` as a tinct function: call `regex-match?` for `pattern`, recurse on `fields:` and `items:` entries, collect violations into a Seq; remove `validate_value` from `src/builtins_meta.rs` (`stdlib/prelude.llt`, `src/builtins_meta.rs`)
- [ ] Keep `validate` registered as a thin Rust stub that calls the tinct function and maps errors to `SchemaViolation` error kind (`src/builtins_meta.rs`)
- [ ] Tests: all existing `validate` corpus tests pass after rewrite; validate over 1000-entry dict completes in <100ms (`tests/corpus/eval/`)

---

## Standard Library Boundary

### meta-primitives-wrapper: Tinct wrappers for meta/variant/numeric Rust primitives

Eight user-facing primitives (`eval-ast`, `gensym`, `llt-repr`, `tag-of`, `variant`, `decimal`, `big-int`, `proxy`) currently leak from `standard_builtins()` directly into every user environment without tinct wrappers. Per the stdlib-boundary principle, Rust functions should not reach user contexts directly — each should be wrapped in tinct and the raw Rust names accessible only via `%rust "meta"` / `%rust "math"` for prelude-internal use.

- [ ] Remove `eval-ast`, `gensym`, `llt-repr` from direct `standard_builtins()` top-level registration; keep them accessible via `[include %rust "meta"]` only; update any Rust call-sites that depend on the global name (`src/builtins.rs`)
- [ ] Remove `tag-of`, `variant` from direct top-level registration; keep in `%rust "meta"` module only (`src/builtins.rs`)
- [ ] Remove `decimal`, `big-int` from direct top-level registration; keep accessible via `%rust "math"` or a new `%rust "numeric"` group (`src/builtins.rs`)
- [ ] Remove `proxy` from direct top-level registration; keep in `%rust "meta"` only (`src/builtins.rs`)
- [ ] Add one-line tinct wrapper functions in `stdlib/prelude.llt` for each of the 8 names, using the `%rust` module name after `[include %rust "meta"]` / `[include %rust "math"]` (`stdlib/prelude.llt`)
- [ ] Verify `src/type_env.rs` type registrations: schemes for the 8 names must still resolve correctly after the wrapper indirection; update if they were keyed to the direct Rust registration (`src/type_env.rs`)
- [ ] Tests: `[gensym]`, `[gensym "prefix"]`, `[eval-ast ...]`, `[llt-repr ...]`, `[tag-of ...]`, `[variant ...]`, `[decimal ...]`, `[big-int ...]`, `[proxy ...]` all work from user code via prelude wrappers; `%rust "meta"` include gives prelude access to underlying names; all existing corpus tests pass (`tests/corpus/eval/`)

---

## Codebase Health (Review #7, 2026-05-16)

### health-review7: Type system soundness, test coverage, docs

Findings from full-panel codebase review (Cycle #246, 2026-05-16).

- [x] **[Critical]** `src/types.rs:641-657` — `is_consistent` TypeVar unsoundness: `(Type::TypeVar(_, _), _) | (_, Type::TypeVar(_, _)) => true` treats ALL TypeVars as consistent with everything, creating a non-transitive abuse where `α ~ Int` and `α ~ Str` both hold. Comment in code says "UNSOUND". Fix: restrict to reflexivity `(Type::TypeVar(n1, _), Type::TypeVar(n2, _)) => n1 == n2`, fix all callers to apply substitution before calling `is_consistent` (type-theorist)
- [x] **[Critical]** `src/type_unify.rs:214-242` — `is_superclass_of` has no cycle guard: if user declares `[A: [class [...] extends [B]]]` and `[B: [class [...] extends [A]]]`, this function loops forever consuming stack. Fix: add `visited: &mut HashSet<String>` parameter OR validate DAG acyclicity at `ClassDecl` registration time (computer-scientist, Jones 1992)
- [x] **[Major]** `src/type_unify.rs:263` — `check_constraints_on_var` clones the entire `Vec<Constraint>` on every type variable binding (`state.constraints.clone()`). Makes every U-VAR binding O(c) where c = constraint count. Fix: index constraints by variable name via `HashMap<String, Vec<Constraint>>` for O(1) amortized lookup (computer-scientist)
- [x] **[Major]** `src/typecheck_dict.rs:143-222` — `collect_deps_recursive` converted to iterative worklist `collect_dependencies` matching the iterative Tarjan pattern (computer-scientist)
- [x] **[Major]** `src/type_env.rs:1716-3344` — 80+ builtin signatures use `Type::Unknown` where precise types are possible. **AUDIT IN PROGRESS** (2026-05-16): Completed first pass — replaced Unknown with precise types for 20+ signatures. See `unknown-elimination` sprint below for remaining work.
- [x] **[Major]** `src/type_unify.rs:441-497` — MPTC improvement lookup `lookup_arithmetic_instance` is a hardcoded match for Add/Sub/Mul/Div only. User-defined MPTCs with fundeps silently fail improvement. Fix: generalize to `instance_env` lookup as documented in the TODO comment at line 433 (type-theorist)
- [x] **[Major]** `tests/corpus/eval/errors/` — E055/E056/E057 (`IncludeHashMismatch`, `IncludeHashRequired`, `IncludePathNotAllowed`) have zero corpus tests. Fix: add 3 corpus tests exercising include integrity error paths (test-crafter)
- [x] **[Major]** `tests/corpus/eval/errors/` — No corpus test for non-cacheable error restoration: `DepthExceeded` should NOT cache in `Failed` state but there is no end-to-end corpus validation. Fix: add test calling a builtin that raises DepthExceeded, verify error is not memoized (test-crafter)
- [x] **[Major]** `tests/test_helpers.rs:53` — `split_test_file()` core test infrastructure function has zero unit tests. Fix: add `tests/test_helpers_test.rs` with ~10 cases: bare `===` error, valid labeled sections, directive parsing, empty sections, unknown label (test-crafter)
- [x] **[Major]** `src/error.rs:629-850` — `ErrorKind` `PartialEq` is a 36-arm manual match. Adding a new variant compiles but produces wrong equality — the new arm silently falls through. Fix: replace with a compile-time exhaustiveness checker or use `#[derive(PartialEq)]` with structural equality (integration-verifier)
- [x] **[Major]** `doc/11-stdlib.md:326`, `doc/11a-builtins.md:4,725` — Stale builtin count: docs say 184 or 189, actual count from `src/builtins.rs` is ~215. Fix: update all three locations; add a CI check or comment with how to recount (stdlib-author)
- [x] **[Major]** `doc/11b-reference.md` — 10-line stub claiming to be "auto-generated from `@[doc]` annotations" but is hand-written with 3 entries. Fix: either implement actual auto-generation from `@[doc]` annotations in `stdlib/*.llt`, or delete and merge the 3 module links into `doc/11-stdlib.md` (stdlib-author)
- [x] **[Minor]** `src/eval.rs` (CEK `Vec<Cont>`) — Iterative CEK continuation stack has no explicit depth limit. Recommend `MAX_CONTINUATION_STACK=2048` guard for defense-in-depth against adversarial inputs (security-expert)
- [x] **[Minor]** `src/lsp/document.rs` — `load_doc_from_uri` (hover/goto-def on unopened files) reads file without checking size first. Fix: check file size against `MAX_FILE_SIZE` before reading, matching the existing guard on document updates (security-expert)
- [x] **[Minor]** `src/type_unify.rs:1042-1115` — `lower_levels_check_occurs` uses `_ => false` catch-all which will silently miss new compound `Type` variants, breaking level-lowering invariant (Kiselyov 2013). Fix: replace with exhaustive match listing all leaf types; Rust will emit a compile error when a new variant is added (computer-scientist)
- [x] **[Minor]** `src/type_unify.rs:1890-2077` — C-Var1/C-Var2 (Union/Intersection with one TypeVar) unification arms duplicated ~200 lines for left/right directions. Fix: extract `bind_single_type_var_from_compound(members, concrete, subst, state, span, is_union)` helper (computer-scientist)
- [x] **[Minor]** `tests/corpus/eval/cross_feature/` — Cross-feature interaction tests are sparse. Fix: add 6 tests minimum: TypeAssert+Proxy, TypeAssert+documents, `$_`+TypeAssert, access chains+Guarded thunks, Proxy+laziness, documents+scope+TypeAssert (test-crafter)
- [x] **[Minor]** `stdlib/prelude.llt:various` — `flatten`, `from-entries`, `group-by`, `deep-merge`, `walk`, `transpose`, `uniq` are O(n²) due to repeated `merge`/`concat` in `reduce` but have no performance warning in their `@[doc]` strings. Fix: add "O(n²) due to repeated merge/concat — use with care on large collections" to each (stdlib-author)

### unknown-elimination: Replace remaining `Type::Unknown` builtin signatures with precise types

First-pass audit complete (2026-05-16). The following categories of Unknown remain and require future work:

**Category B — TypeVar polymorphism required (HKT or multi-arity):**
- `map`, `filter`, `reduce`: target `∀f a b. Mappable f => (a→b)→f a→f b`. Requires higher-kinded types (Type::App) not yet representable in TypeScheme. See comment `// TODO(unknown-elimination)` in each signature.
- `each`, `each-key`, `each-kv`: return element type requires HKT over input collection type.
- `builtin-collect`: `Seq(Unknown)` param; return Dict erases element type anyway — low priority.

**Category A — Record return types (closed Record schema needed):**
- `revocable`: returns `{cap: DirCap, revoke: Fn()->Null}` — expressible once Rust builtin signatures support closed Record return types.
- `recv-datagram`: returns `{data: Bytes, addr: Str, port: Int}`.
- `tls-peer-cert`: returns `{subject: Str, issuer: Str, sans: Seq(Str), ...}`.
- `icmp-ping`: returns `{rtt_ms: Int, success: Bool}`.
- `http-request`: returns `{status: Int, headers: Map(Str,Str), body: Bytes}`.
- `list-dir`: returns `Seq({name: Str, kind: Str, size: Int, ...})`.
- `stat`: returns `{name: Str, kind: Str, size: Int, ...}`.
- `timestamp-parts`: returns `{year: Int, month: Int, day: Int, hour: Int, minute: Int, second: Int}`.
- `timestamp-in-tz`: returns the above plus `offset-seconds: Int, tz-name: Str`.
- `builtin-first`/`builtin-last`: return type depends on input type (Dict element, Str char, Int byte).

**Category A — Genuinely unknown (no precise type possible without language feature):**
- `from-json`: requires schema-directed parsing; return is `Unknown` by design.
- `include`: included file type not knowable without parsing the included file at type-check time.
- `builtin-get`/`get?`: special-cased by `check_get` dispatcher; performance constraint prevents polymorphic registration.
- `map`/`filter`/`reduce` seq/init params: HKT required.
- `builtin-join` seq param: `stringify()` accepts any element type.
- `builtin-concat` return: merge shape not inferrable statically.
- Transport variant constants (`Tcp`, `Udp`, etc.): requires `Type::Variant`.
- `connect` transport param: requires `Type::Variant` for dispatch.
- `Map` unparameterized constructor: `Unknown` K/V until user supplies type args.

**Tasks:**
- [ ] Implement `Type::Variant` and replace Transport constant `Unknown` registrations (`src/type_env.rs`, `src/types.rs`)
- [ ] Add closed-Record return type for `revocable`, `icmp-ping`, `recv-datagram`, `stat`, `timestamp-parts`, `timestamp-in-tz`, `timestamp-in-tz`, `tls-peer-cert`, `http-request` (`src/type_env.rs`)
- [ ] Add precise `Seq({...})` return for `list-dir` once Seq(Record) return is supported
- [ ] Implement HKT (`Type::App`) to express `map`/`filter`/`reduce`/`each` precisely — see `chr-unification` sprint for the type-application machinery
- [ ] After above: add `from-json` option for schema-directed typed parse returning a specific Record type

---

## Prelude Annotation Modernization

Modernize `stdlib/prelude.llt` to use the full annotation and typing infrastructure added in sprints up to 2026-05-16. Currently ~126 public functions use bare `fn@Type` return annotations and `name@[doc: "..."]` key annotations with comment blocks above them. The goal is a single `fn@[return: T  constraint: [...]  doc: "..."]` metadata dict per function that replaces both the `name@[doc: "..."]` key annotation and the `# Type:` / `# Example:` / `# NOTE:` comment block above it.

**Annotation conventions:**
- `fn@[return: T  constraint: [a: Comparable]  doc: "One-line desc.\n\nExample: [fn arg] => result\n\nNote: edge case"]` — full form
- `doc:` string: one-line summary, blank line, then `Example:` lines, then `Note:` lines (from existing comments); verbatim text from the existing comment block
- Param annotations: upgrade `@Fn` → `@[return: R]` or `@[a b -> R]` where the param function's signature is known; upgrade `@Dict` → `@Seq@T` or `@Map@[K: V]` where the concrete collection type is known; use `@Label` for `get`/`get-or`/`get?`/`builtin-get` key params
- Private helpers (`-impl`, `-step`, `-check` suffix): fix type annotations but skip doc migration (internal); no `doc:` string needed
- Skip functions that already have `fn@[return: T  constraint: [...]]` form unless adding `doc:` improves them materially

**Sprint split:** This sprint is intentionally large. Split into 4 sub-sprints at planning time if > 30 non-nit tasks per sub-sprint:

### prelude-annotations-a: Identity, Logic, Comparison, Arithmetic, Numeric conversion

Public functions in prelude.llt lines ~357–550. ~25 functions:
`identity`, `const`, `not`, `and`, `or`, `any?`, `all?`, `>`, `<=`, `>=`, `quot`, `mod`, `ceil`, `trunc`, `abs`, `sign`, `clamp`, `min`, `max`, `sum`, `product`, `average`, `gcd`, `lcm`, `between?`.

- [ ] Migrate `identity`, `const`: consolidate `name@[doc: "..."]` + bare `fn` into `fn@[return: a  doc: "..."]`; drop comment block (`stdlib/prelude.llt`)
- [ ] Migrate `not`: `fn@[return: Bool  doc: "Boolean negation.\n\nExample: [not true] => false"]`; drop comment block (`stdlib/prelude.llt`)
- [ ] Migrate `and`, `or`: full metadata dict with `doc:` including `# NOTE:` content about lazy evaluation semantics; drop comment blocks (`stdlib/prelude.llt`)
- [ ] Migrate `any?`, `all?`: add `doc:` with type, examples, and materialization note; param `pred` upgrade from `@Fn` to `@[return: Bool]` if supported (`stdlib/prelude.llt`)
- [ ] Migrate `>`, `<=`, `>=`: already have `fn@[return: Bool constraint: [a: Comparable]]`; add `doc:` string from comment block (`stdlib/prelude.llt`)
- [ ] Migrate `quot`, `mod`: `fn@[return: Int  doc: "..." ]`; include semantics note (truncation direction, sign of remainder); drop comment blocks (`stdlib/prelude.llt`)
- [ ] Migrate `ceil`, `trunc`, `abs`, `sign`, `clamp`: `fn@[return: T  constraint: [a: Numeric]  doc: "..."]`; include examples from comment blocks (`stdlib/prelude.llt`)
- [ ] Migrate `min`, `max`: `fn@[return: a  constraint: [a: Comparable]  doc: "..."]`; include empty-collection behavior note (`stdlib/prelude.llt`)
- [ ] Migrate `sum`, `product`: `fn@[return: Number  doc: "..."]`; include empty-collection base-case note (`stdlib/prelude.llt`)
- [ ] Migrate `average`, `gcd`, `lcm`, `between?`: full metadata dict with examples from comment blocks (`stdlib/prelude.llt`)
- [ ] Verify `just test-lib` passes after each batch; fix any annotation-inference regressions (`stdlib/prelude.llt`)

### prelude-annotations-b: Collection and Dict operations

Public functions in prelude.llt, collection section. ~25 functions:
`length`, `keys`, `values`, `entries`, `has?`, `get`, `get-or`, `get?`, `get-in`, `get-in-or`, `remove`, `remove-keys`, `keep-keys`, `reindex`, `merge-with`, `group-by`, `frequencies`, `index-by`, `map-entries`, `map-keys`, `map-values`, `flat-map`, `zip`, `unzip`, `partition`, `take-while`, `drop-while`, `sliding`, `chunks`.

- [ ] Migrate `get`, `get-or`, `get?`: use `@Label` for `key` param; `fn@[return: a  doc: "..."]` with HasField constraint generated automatically; include key-not-found behavior note (`stdlib/prelude.llt`)
- [ ] Migrate `get-in`, `get-in-or`: doc includes path-traversal semantics and `Null`-propagation note (`stdlib/prelude.llt`)
- [ ] Migrate `has?`, `remove`, `remove-keys`, `keep-keys`: full metadata dict with examples; `@Seq@Str` for key-list params where applicable (`stdlib/prelude.llt`)
- [ ] Migrate `keys`, `values`, `entries`: `fn@[return: Seq@T  doc: "..."]` where element type is known; include ordering note (insertion order) (`stdlib/prelude.llt`)
- [ ] Migrate `reindex`, `merge-with`, `group-by`, `frequencies`, `index-by`: full metadata dict with examples from comment blocks (`stdlib/prelude.llt`)
- [ ] Migrate `map-entries`, `map-keys`, `map-values`: upgrade `pred@Fn` to `pred@[return: T]` or `pred@[k v -> T]` where the arity is fixed (`stdlib/prelude.llt`)
- [ ] Migrate `flat-map`, `zip`, `unzip`, `partition`, `take-while`, `drop-while`, `sliding`, `chunks`: full metadata dict with examples; include materialization notes (`stdlib/prelude.llt`)
- [ ] Verify `just test-lib` passes; fix any regressions (`stdlib/prelude.llt`)

### prelude-annotations-c: Sequences, Strings, Control flow, Error handling

Public functions: `range`, `repeat`, `iterate`, `cycle` (seq); `str-join`, `str-split`, `str-trim`, `str-pad-left`, `str-pad-right`, `str-replace`, `str-find`, `str-reverse`, `format`, `parse-int`, `parse-float` (string); `if`, `cond`, `when`, `unless`, `try`, `error`, `assert` (control); `->`, `|>`, `compose`, `flip`, `partial` (combinators).

- [ ] Migrate sequence generators `range`, `repeat`, `iterate`, `cycle`: `fn@[return: Seq@T  doc: "..."]`; include laziness note; `range` variadic upgrade from `@Unknown` to `@Seq@Int` already done — verify and add `doc:` (`stdlib/prelude.llt`)
- [ ] Migrate string functions `str-join`, `str-split`, `str-trim`, `str-pad-left`, `str-pad-right`, `str-replace`, `str-find`, `str-reverse`: `fn@[return: Str  doc: "..."]`; include Unicode/codepoint behavior notes (`stdlib/prelude.llt`)
- [ ] Migrate `format`, `parse-int`, `parse-float`: include failure behavior in `doc:` (parse returns `Null` or `Err` on failure) (`stdlib/prelude.llt`)
- [ ] Migrate `cond`, `when`, `unless`: include lazy-branch note in `doc:` (`stdlib/prelude.llt`)
- [ ] Migrate `try`, `error`, `assert`: include error propagation semantics in `doc:` (`stdlib/prelude.llt`)
- [ ] Migrate combinators `->`, `|>`, `compose`, `flip`, `partial`: `fn@[return: Fn  doc: "..."]`; include arity notes (`stdlib/prelude.llt`)
- [ ] Verify `just test-lib` passes; fix any regressions (`stdlib/prelude.llt`)

### prelude-annotations-d: Result monad, HKT hierarchy, Typeclass instances

Public functions: `Ok`, `Err`, `ok?`, `err?`, `and-then`, `result-or`, `result-map`, `result-ok`; `Functor`/`Applicative`/`Monad`/`Foldable`/`Traversable` class declarations; `FunctorSeq`/`MonadResult`/etc. instance declarations; `Maybe`, `Some`, `None`; `sequence`, `traverse`, `forM`, `liftM2`, `whenM`; `Equatable`/`Comparable`/`Showable`/`Mappable`/`Appendable` class and instance declarations.

- [ ] Migrate `Ok`, `Err`, `ok?`, `err?`: `doc:` string explaining `Result = Ok[a] | Err[Str]` nominal type, constructor usage, and predicate semantics (`stdlib/prelude.llt`)
- [ ] Migrate `and-then`, `result-or`, `result-map`, `result-ok`: `doc:` strings including monad-law descriptions; `and-then` is `MonadResult.bind` — note the equivalence (`stdlib/prelude.llt`)
- [ ] Add `doc:` to class declarations (`Functor`, `Applicative`, `Monad`, `Foldable`, `Traversable`, `Mappable`, `Appendable`, `Equatable`, `Comparable`, `Showable`): one-line description of the abstraction and the laws it must satisfy (`stdlib/prelude.llt`)
- [ ] Add `doc:` to instance declarations (`FunctorSeq`, `FunctorResult`, `MonadResult`, `MonadSeq`, etc.): one-line description of what each instance does (`stdlib/prelude.llt`)
- [ ] Add `doc:` to `Maybe`, `Some`, `None`: explain optional value semantics, contrast with `Null` (`stdlib/prelude.llt`)
- [ ] Migrate `sequence`, `traverse`, `forM`, `liftM2`, `whenM`: `doc:` includes type description and example; note `sequence = [fn [t] [traverse t id]]` identity (`stdlib/prelude.llt`)
- [ ] Verify `just test-lib` passes; fix any regressions (`stdlib/prelude.llt`)

---
