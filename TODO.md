# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## CHR Unification

`chr-unification` accepted 2026-05-16 (commits 0886ef1, 7d15c36). See `doc/whatif/chr-unification.md` and `doc/feature/chr-unification.md`. Implementation order: chr-module-split → chr-normalization → chr-class-instance → chr-prelude.


### chr-class-instance: AST redesign and parser/typecheck support for [class] and [instance]

Redesigns `Expr::ClassDecl` and `Expr::InstanceDecl` for the two-bracket class body and match-arm instance syntax. New `[pattern [...]]` form reuses existing annotated-identifier machinery.

- [ ] Extend `Expr::ClassDecl` in `src/ast.rs`: add `determines: Vec<Spanned<Expr>>`, `resolver: Option<Spanned<Expr>>`; update all exhaustive match sites (`src/eval.rs`, `src/typecheck.rs`, `src/formatter.rs`, `src/desugar.rs`, `src/resolve.rs`, `src/lsp/analysis.rs`, `src/ast_dict.rs`, `src/expand.rs`) (`src/ast.rs` + ~8 files)
- [ ] Update `StackFrame::ClassDecl` in `src/parser.rs`: add `structural_metadata: Option<Spanned<Expr>>`; route second positional `Expr::Dict` to `structural_metadata` (currently hard-errors at `parser.rs:~4819–4827`); extract `determines:`, `resolver:`, `kinds:`, `superclasses:` from structural metadata entries in `CloseBracket` ClassDecl handler; retire `f@Operator` form in class param lists (`src/parser.rs`)
- [ ] Redesign `Expr::InstanceDecl` in `src/ast.rs`: from `{class_name, instance_type, methods}` to `{class_name, arms: Vec<(Spanned<Expr>, Vec<Spanned<Entry>>)>}`; update all exhaustive match sites (~8 files); update `InstanceDecl` Display to render match-arm form (`src/ast.rs` + ~8 files)
- [ ] Update `StackFrame::InstanceDecl` in `src/parser.rs`: replace `instance_type`/`methods` with `arms`/`pending_arm_key`; add `VarRef` branch for bare class name; handle `[pattern [...]]` arm key / `:` separator; require method bodies as bracket forms (inner `StackFrame::Dict` delivers completed `Expr::Dict`) (`src/parser.rs`)
- [ ] Add `Expr::PatternDecl { bindings: Vec<Spanned<Expr>> }` to `src/ast.rs` + `StackFrame::PatternDecl` in `src/parser.rs`: `pattern` keyword recognition (with colon-ahead rejection guard `!matches!(peek_next_horizontal(...), Some((Token::Colon, _)))`); collects `Expr::Annotated` nodes from inner Dict frame into `bindings`; no body (`src/ast.rs`, `src/parser.rs`)
- [ ] Implement `Expr::ClassDecl` typecheck handler in `src/typecheck.rs`: validate `determines:` 2-element list structure; resolve param names to positional indices; check `resolver` name exists in type-stage Env; validate coverage condition; compute `resolver_injective` during batch instance coherence check; validate consistency condition (`src/typecheck.rs`)
- [ ] Implement `Expr::InstanceDecl` typecheck handler in `src/typecheck.rs`: validate arm type-parameter count matches class params; pairwise disjointness check across arms; coverage and consistency checks for FD classes; register arms in scope-local InstanceEnv (TypeEnv entry, not global HashMap); typecheck each method impl against class method signature with arm's type params substituted (`src/typecheck.rs`)
- [ ] Tests: basic `[class [a b c] [determines: ...] +: [fn@c [a b]]]` + `[instance ...]` declaration; FD inference at call site; disjointness violation error message; coverage violation error; consistency violation error; method type mismatch error (`tests/corpus/eval/typecheck/`)

### chr-prelude: Migrate arithmetic classes to prelude.llt and implement boundary guard elaboration

Moves the hardcoded arithmetic instance table out of Rust and into tinct itself. Completes the CHR cycle by adding the post-inference boundary guard elaboration pass.

- [ ] Add iteration cap (e.g. 100) to `process_deferred_equalities()` in `src/type_unify.rs` before resolver evaluation activates (safety against unbounded fixed-point iteration) (`src/type_unify.rs`)
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
- `builtin-get`/`get?`: special-cased by `check_get` dispatcher; performance constraint prevents polymorphic registration.
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

## Prelude Annotation Modernization

### prelude-triple-quote: Migrate prelude doc: strings to triple-quoted form

`"""..."""` is fully implemented (lexer `Token::TripleQuotedString`, parser desugars to `[unindent "..."]`). The `doc:` strings in `stdlib/prelude.llt` currently use `\n` escape sequences in regular double-quoted strings. Replace with `"""` for readability.

- [ ] Replace all `doc: "...\n\n..."` multi-line strings in `stdlib/prelude.llt` with `doc: """..."""` triple-quoted form; use natural indentation for Example: and Note: sections (`stdlib/prelude.llt`)
- [ ] Verify `just test-lib` passes; doc string content unchanged (`stdlib/prelude.llt`)

---
