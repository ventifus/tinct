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

- [ ] Add iteration cap (e.g. 100) to `process_deferred_equalities()` in `src/type_unify.rs` before resolver evaluation activates (safety against unbounded fixed-point iteration) (`src/type_unify.rs`)
- [ ] Add corpus test for `determines:` extraction round-trip before wiring up chr-prelude resolvers; verify `[class [a b c] [determines: [[[a b] c]]] ...]` correctly populates ClassDecl.determines (`tests/corpus/eval/typecheck/`)
- [ ] Fix consistency check to use unify-under-θ instead of structural equality (`types_equal`) — currently overly conservative for parametric instance types (`src/typecheck.rs:2400`)
- [ ] Improve disjointness/consistency error spans: include both conflicting arm spans, not just the second one (`src/typecheck.rs:2326,2401`)
- [ ] Coverage error message: use param name from `params` list instead of zero-based index (`src/typecheck.rs:2362`)
- [ ] Remove backward-compat legacy instance parsing (`legacy_arm_pattern` field in `StackFrame::InstanceDecl`, `push_expr_to_parent` legacy branch in parser.rs ~line 5032) — only valid after all `[instance [ClassName Type] ...]` forms in `stdlib/prelude.llt` and corpus tests are migrated to `[instance ClassName [pattern [...]] ...]` form (`src/parser.rs`)
- [ ] Write `AddResult`, `SubResult`, `MulResult`, `DivResult` resolver functions in `--- stage: type` section of `stdlib/prelude.llt`; write `Addable`, `Subtractable`, `Multipliable`, `Divisible` class declarations with `determines:/resolver:` and all instance arms (Int×Int, Int×Float, Float×Int, Float×Float) (`stdlib/prelude.llt`)
- [ ] Remove `lookup_arithmetic_instance` from `src/type_unify.rs`; pre-populate `NormCtxt` normalization cache with the 9 arithmetic results as the O(1) fast path (keyed by resolver name + type-dict args) (`src/type_unify.rs`)
- [ ] Implement `elaborate_boundary_guards()` post-inference elaboration pass in `src/typecheck.rs` or new `src/typecheck_elaborate.rs`: walk type map after inference completes; for each expression where inferred type is `Unknown` and contextual expected type is concrete, call `normalize(τ_ctx, NormCtxt::final(...))` and annotate the expression with the concrete expected type; emit `TypeError` if expected type is still irreducible after normalization (`src/typecheck.rs` or `src/typecheck_elaborate.rs`)
- [ ] Implement eval-side guard creation in `src/eval.rs`: when an expression has a concrete expected-type annotation (written by the elaboration pass), wrap its thunk in `ThunkState::Guarded`; `BlameLabel` carries `origin_span`, `boundary_span`, `polarity` (Negative for call args, Positive for return-value consumers) (`src/eval.rs`)
- [ ] Tests: arithmetic FD inference for all 4 operations and all Int/Float combinations; user-defined `Decimal` class instance with `[+ dec1 dec2]` → `Decimal`; boundary guard inserted at Unknown→Int boundary; blame label carries correct origin span; `[from-json input].port` fires guard if `start` expects `Int` (`tests/corpus/eval/typecheck/`, `tests/corpus/eval/`)


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
