# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## CHR Unification

`chr-unification` accepted 2026-05-16 (commits 0886ef1, 7d15c36). See `doc/whatif/chr-unification.md` and `doc/feature/chr-unification.md`. Implementation order: chr-module-split → chr-normalization → chr-class-instance → chr-prelude.


### chr-class-instance: AST redesign and parser/typecheck support for [class] and [instance]

Redesigns `Expr::ClassDecl` and `Expr::InstanceDecl` for the two-bracket class body and match-arm instance syntax. New `[pattern [...]]` form reuses existing annotated-identifier machinery.

- [x] Extend `Expr::ClassDecl` in `src/ast.rs`: add `determines`, `resolver` fields; update all exhaustive match sites (`src/ast.rs` + ~8 files)
- [x] Update `StackFrame::ClassDecl` in `src/parser.rs`: structural_metadata second bracket; extract determines/resolver/kinds/superclasses; key extraction handles VarRef+Str (`src/parser.rs`)
- [x] Redesign `Expr::InstanceDecl` in `src/ast.rs`: arms Vec form; backward-compat legacy_arm_pattern; update all exhaustive match sites (`src/ast.rs` + ~8 files)
- [x] Update `StackFrame::InstanceDecl` in `src/parser.rs`: arms/pending_arm_pattern; pattern arm syntax; legacy support (`src/parser.rs`)
- [x] Add `Expr::PatternDecl { bindings }` to `src/ast.rs` + `StackFrame::PatternDecl`; colon-ahead rejection guard (`src/ast.rs`, `src/parser.rs`)
- [x] Implement ClassDecl typecheck: determines/resolver validation, coverage, consistency; 6-field probe isolation in patterns_overlap (`src/typecheck.rs`)
- [x] Implement InstanceDecl typecheck: disjointness, coverage, consistency, InstanceEnv registration, all-arms iteration, VarRef method keys (`src/typecheck.rs`, `src/eval.rs`)
- [x] Tests: class_fd_basic.llt-eval, instance_pattern_basic.llt-eval, instance_legacy_syntax.llt-eval; unit test for FD consistency violation (`tests/corpus/eval/typecheck/`, `src/lib.rs`)

### chr-prelude: Migrate arithmetic classes to prelude.llt and implement boundary guard elaboration

Moves the hardcoded arithmetic instance table out of Rust and into tinct itself. Completes the CHR cycle by adding the post-inference boundary guard elaboration pass.

- [x] Add iteration cap (100) to `process_deferred_equalities()` (`src/type_unify.rs`)
- [x] Add corpus test for `determines:` extraction round-trip (`tests/corpus/eval/typecheck/class_determines_roundtrip.llt-eval`)
- [ ] Fix consistency check to use unify-under-θ instead of structural equality (`types_equal`) — deferred, O(N²) performance risk for large prelude (`src/typecheck.rs:2400`)
- [x] Improve disjointness/consistency error spans: both arm spans included (`src/typecheck.rs`)
- [x] Coverage error message: uses param name from `params` list (`src/typecheck.rs`)
- [x] Add `instance_resolution_depth: u32` to `InferState`; guard `resolve_instance` call in `check_constraints_on_var` (limit 64, matching GHC `-freduction-depth` per Sulzmann et al. 2007 §3.2); **unblocks all remaining chr-prelude and unified-bindings-migrate work** (`src/type_unify.rs`, `src/type_infer.rs`)
- [x] Add `in_prelude_load: bool` flag to `InferState`; skip InstanceDecl method body inference during prelude load (`src/type_infer.rs`, `src/typecheck.rs`, `src/imports.rs`)
- [x] Wire boundary guards from typecheck to eval pipeline: `boundary_guards` on EvalContext, `set_boundary_guards()` method; wired in `eval_source_with_config`, `eval_source_with_cap_net`, `run_eval` (`src/eval.rs`, `src/lib.rs`, `src/main.rs`)
- [ ] Remove backward-compat legacy instance parsing — after instance_resolution_depth guard is in and all prelude instances migrated (`src/parser.rs`)
- [ ] Write resolver functions + arithmetic class declarations in prelude.llt; activate `improve_functional_dependency` cache path — requires instance_resolution_depth guard first (`stdlib/prelude.llt`, `src/type_unify.rs`)
- [x] NormCtxt resolver_cache pre-populated (16 entries); `improve_functional_dependency` has `fd_depth` guard with `MAX_FD_DEPTH=16` (`src/type_normalize.rs`, `src/type_unify.rs`)
- [x] `boundary_guards: Vec<(Span, Type)>` added to InferState; collected at CALL-MONO and CALL-POLY boundaries (`src/type_infer.rs`, `src/typecheck.rs`)
- [ ] Wire boundary guards to eval: create guarded thunks from `state.boundary_guards`; eval-side `ThunkState::Guarded` with BlameLabel (`src/eval.rs`)
- [ ] Tests: full arithmetic FD + boundary guard tests (blocked on resolver activation)

---

## Unified Binding Declarations

`unified-bindings` accepted 2026-05-17. See `doc/whatif/unified-bindings.md` and `doc/02-syntax.md` §6, §9. Implementation order: unified-bindings-ast → unified-bindings-typecheck → unified-bindings-migrate.

- [x] Design unified bindings — see doc/whatif/unified-bindings.md

### unified-bindings-ast: Lexer, AST, and parser for [let ...], [case ...], and ... placeholder

Add `Token::Let`, `Token::Case`, `Expr::LetDecl`, `Expr::CaseArm`, `Expr::Placeholder` and parser support. Both old and new binding syntax accepted during this phase (old syntax deprecated but functional to avoid breaking everything at once). **Spec chapters:** `doc/02-syntax.md §6, §9`, `doc/whatif/unified-bindings.md §src/lexer.rs, §src/ast.rs, §src/parser.rs`.

- [ ] Add `Token::Let` and `Token::Case` keywords to `src/lexer.rs`; add both to the reserved keyword denylist (`src/lexer.rs`)
- [ ] Add `Expr::LetDecl { bindings: Vec<Spanned<Expr>> }`, `Expr::CaseArm { pattern: Box<Spanned<Expr>>, body: Box<Spanned<Expr>> }`, `Expr::Placeholder` to `src/ast.rs`; update all exhaustive match sites (`src/ast.rs`, `src/desugar.rs`, `src/formatter.rs`, `src/expand.rs`, `src/resolve.rs`, `src/ast_dict.rs`)
- [ ] Add `StackFrame::LetDecl` to `src/parser.rs`; add `StackFrame::CaseDecl` (`src/parser.rs`)
- [ ] Parse `Expr::Placeholder`: `Token::Spread` not followed by `Token::Identifier` in value position (`src/parser.rs`)
- [ ] Add `let:` and `case:` colon-ahead disambiguation to keyword dispatch table (`src/parser.rs`)
- [ ] Update `StackFrame::Fn`, `StackFrame::ClassDecl`, `StackFrame::TypeAlias`, `StackFrame::InstanceDecl`, `StackFrame::Match` to accept `[let ...]` / `[case ...]` forms (keep old paths functional) (`src/parser.rs`)
- [ ] Tests: parser tests for new binding syntax, `...` placeholder, colon-ahead disambiguation (`tests/corpus/eval/`, `src/lib.rs`)

### unified-bindings-typecheck: Type checker and evaluator for binding declarations, case arms, and placeholders

**Depends on:** `unified-bindings-ast`

- [ ] Implement binding extraction from `Expr::LetDecl` in fn/class/type/instance/case contexts (`src/typecheck.rs`)
- [ ] Implement `typecheck_case_arm`: constructor payload lookup, type narrowing (`[let n@T]` → `n : scrutinee_ty ∩ T`) (`src/typecheck.rs`)
- [ ] Implement `Expr::LetDecl` validity check and structural-test restriction (`src/typecheck.rs`)
- [ ] Type `Expr::Placeholder` as `Unknown` (`src/typecheck.rs`)
- [ ] Implement `eval_case_arm`, `eval_let_pattern`, nullary variant `values_equal`, `Expr::Placeholder` eval (`src/eval.rs`, `src/value.rs`)
- [ ] Tests: case arm type narrowing, LetDecl-in-expression-position error, Placeholder eval (`tests/corpus/eval/`, `src/lib.rs`)

### unified-bindings-migrate: Migrate all existing code to [let ...] and [case ...] syntax

**Depends on:** `unified-bindings-typecheck`

- [ ] Migrate ~242 fn declarations in `stdlib/prelude.llt` to `[fn [let params] body]` (`stdlib/prelude.llt`)
- [ ] Migrate class/type/instance declarations in `stdlib/prelude.llt` (`stdlib/prelude.llt`)
- [ ] Migrate all corpus test files and doc examples (`tests/corpus/`, `doc/`)
- [ ] Remove old param-list parsing paths (old syntax becomes a parse error) (`src/parser.rs`)
- [ ] Verify `just test` passes with all migrations applied (`tests/`)

---

## Codebase Health

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
- `builtin-get`/`get?`: special-cased by `check_get` dispatcher; label-polymorphic scheme (`HasField l d a`) was attempted but reportedly caused inference to hang on prelude.llt (informal O(N²) analysis: ~35 `get` calls × HasField constraints × substitution merge loop); unproven whether this was a true performance issue or a unification bug — worth re-investigating once chr-class-instance lands a better HasField implementation.
- `map`/`filter`/`reduce` seq/init params: HKT required.
- `builtin-join` seq param: `stringify()` accepts any element type.
- `builtin-concat` return: merge shape not inferrable statically.
- Transport variant constants (`Tcp`, `Udp`, etc.): requires `Type::Variant`.
- `connect` transport param: requires `Type::Variant` for dispatch.
- `Map` unparameterized constructor: `Unknown` K/V until user supplies type args.

**Tasks:**
- [ ] Implement `Type::Variant` and replace Transport constant `Unknown` registrations (`src/type_env.rs`, `src/types.rs`)
- [x] Add closed-Record return type for `revocable`, `icmp-ping`, `recv-datagram`, `stat`, `timestamp-parts`, `timestamp-in-tz`, `timestamp-in-tz`, `tls-peer-cert`, `http-request` (`src/type_env.rs`)
- [x] Add precise `Seq({...})` return for `list-dir` — `Seq({name: Str, kind: Str, size: Int})` (`src/type_env.rs`)
- [ ] Implement HKT (`Type::App`) to express `map`/`filter`/`reduce`/`each` precisely — see `chr-unification` sprint for the type-application machinery
- [ ] After above: add `from-json` option for schema-directed typed parse returning a specific Record type

---

## Test Infrastructure

### corpus-consolidation: Consolidate corpus tests into fewer, more comprehensive test cases

The corpus test suite has grown to hundreds of fine-grained single-feature tests. The goal is to reduce the total number while increasing coverage density per test — each consolidated test should exercise multiple related features together (e.g., a single `arithmetic_mixed_types.llt-eval` that covers Int+Int, Int+Float, Float+Int, Float+Float and their type annotations, rather than 4 separate files). This reduces the serial test execution time (currently 700+ seconds for the full corpus).

**Strategy:** Merge tests within the same subdirectory that share the same builtin or feature area. Keep negative/error tests separate (one file per distinct error code is fine). Target: reduce corpus file count by 30-40%.

- [ ] Audit `tests/corpus/eval/builtins/` — merge arithmetic variants, string operation variants, and type-predicate variants into composite tests (`tests/corpus/eval/builtins/`)
- [ ] Audit `tests/corpus/eval/typecheck/` — merge related positive typecheck tests into 1-3 comprehensive files per feature area (`tests/corpus/eval/typecheck/`)
- [ ] Audit `tests/corpus/eval/stdlib/` — merge related prelude function tests (`tests/corpus/eval/stdlib/`)
- [ ] Verify `just test` passes after consolidation; update any CI time baselines (`tests/`)

---

## Prelude Annotation Modernization

### prelude-triple-quote: Migrate prelude doc: strings to triple-quoted form

`"""..."""` is fully implemented (lexer `Token::TripleQuotedString`, parser desugars to `[unindent "..."]`). The `doc:` strings in `stdlib/prelude.llt` currently use `\n` escape sequences in regular double-quoted strings. Replace with `"""` for readability.

- [ ] Replace all `doc: "...\n\n..."` multi-line strings in `stdlib/prelude.llt` with `doc: """..."""` triple-quoted form; use natural indentation for Example: and Note: sections (`stdlib/prelude.llt`)
- [ ] Verify `just test-lib` passes; doc string content unchanged (`stdlib/prelude.llt`)

---
