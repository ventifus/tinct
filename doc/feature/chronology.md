# Feature Chronology

Features in implementation order, based on git commit dates and DONE.md.
Where a later feature changes the semantics of an earlier one, both are cross-referenced.

Dates are the author date of the completing (last) commit for each feature's sprints.
All commits by Andrew Denton; timezone UTC-7 (Pacific).

---

## 2026-04-29 — Parser Rewrite (Phase 1: Lexer)

- [`4f65c7b`] [Iterative Parser and AST-Based Formatter](parser-rewrite.md) — `parser-lexer`: hand-written iterative lexer (`src/lexer.rs`) replaces Pest PEG tokenizer; whitespace-sensitive access-context tokens

## 2026-04-30 — Parser Rewrite (Phase 2–3: Core + Formatter)

- [`a3a3d08`] [Iterative Parser and AST-Based Formatter](parser-rewrite.md) — `parser-formatter` (completing sprint): AST-based formatter rewrite eliminates token-stream heuristics; source-span quoting detection
  - Commits: `f44a621` (parser-core-a) → `8fcf7b4` (parser-core-b) → `38072fb` (parser-core-c1) → `6021261` (parser-core-c2) → `cc8333c` (parser-core-c3) → `a3a3d08` (parser-formatter)

## 2026-05-01 — Circular Dependency Error Paths

- [`ad23c4c`] [Circular Dependency Error Paths](circular-dep-error-paths.md) — `error-context`: secondary spans at Guarded/`$if` sites; multi-hop cycle paths via `eval_stack`; `$include` chain verified; "did you mean?" suggestions

## 2026-05-02 — New Syntax Reform (Docs)

- [`6f898cd`] [Unified Syntax Reform](new-syntax.md) — `new-syntax-docs` (Phase 0): spec chapters updated to new unified syntax (implied call, bare-word references, `%` pipeline variable, quoted string values); 17 doc files updated

## 2026-05-03 — New Syntax Reform (Migration)

- [`17d9b6d`] [Unified Syntax Reform](new-syntax.md) — `new-syntax-migrate` (completing sprint): final syntax migration and cleanup; README, doc/whatif/*, all `.llt` files migrated; formatter roundtrip fixes
  - Commits: `6f898cd` (new-syntax-docs) → `4b00c3c` (new-syntax-a) → `e9acf9c` (new-syntax-a Phase 1) → `17d9b6d` (new-syntax-migrate)

## 2026-05-04 — Source Text, Arena, Type Predicates, I/O, Templating

All of the following completing commits landed on 2026-05-04:

- [`cef5e1c`] [Source Text Availability](source-text-availability.md) — `source-text` (completing sprint): multi-line span rendering (all spanned lines shown); LSP `related_information` with mat-span + stack frames
  - Phase 1 (CLI snippet rendering) was already done earlier

- [`d8ecf23`] [Type Predicates](type-predicates.md) — `type-predicates`: 8 runtime type predicate builtins (`int?`, `float?`, `num?`, `str?`, `bool?`, `null?`, `dict?`, `fn?`); `list?` in prelude; 32 corpus tests

- [`63ee975`] [Arena Patterns and Flat Environments](arena-patterns.md) — `arena-migrate` (completing Phase 2 sprint): Phase 1 (variable resolution pass, `arena-resolve`, `a092286`) + Phase 2 (arena types + CEK integration) + Phase 2 boundary assessment complete; Phase 3 (full ThunkId migration replacing `Rc<Thunk>`) tracked separately
  - Commits: `a092286` (arena-resolve, Phase 1) → `03aa29b` (arena-types) → `abe0f36` (arena-eval Phase 2 infra) → `7471372` (arena-eval full ThunkId) → `9604953` (arena-cek) → `63ee975` (arena-migrate, Phase 2 boundary assessment)

- [`0183091`] [General I/O](io.md) — `io-phase4` (completing sprint): all 4 I/O phases complete; cap types (`DirCap`, `NetCap`, `Handle`) in type checker; 11 builtin signatures updated
  - Commits: `1248479` (io-phase1) → `a74d4e5` (io-phase1 revocable/lines) → `d1ae363` (io-phase1 complete) → `dd22b85` (io-include-cap) → `17c360a` (io-phase2 network/TCP) → `4cd7a66` (io-phase3 atomic writes) → `0183091` (io-phase4 cap types)

- [`ecdf3b5`] [tinct as a Templating Language](templating.md) — `templating-phase4` (completing sprint): literate mode (tangle/eval/weave); multi-file pipeline; stdlib formatters; string interpolation `i"..."` roundtrip; `${expr}` interpolation; literate Markdown extraction
  - Commits: `ac72be0` (templating-phase1) → `a67e328` (templating-phase2) → `c03ecbc` (templating-phase3) → `645863d` (templating-phase2/3 completion) → `ecdf3b5` (templating-phase4)

## 2026-05-05 — Access Pipeline

- [`0d240b8`] [Unified Access and Generator Pipeline](access-pipeline.md) — `access-pipeline-phase2` (completing sprint): dot extension, `|` desugar, generator builtins (Phase 1); bracket access removed + prelude migration (Phase 2)
  - Commits: `be55da6` (access-pipeline-phase1) → `0d240b8` (access-pipeline-phase2)

## 2026-05-06 — AST Schema, Macros Cluster, Typing Cluster Accepted, let-binding

- [`0b32540`] [Canonical AST Dict Schema](ast-schema.md) — `ast-dict-core`: new `src/ast_dict.rs` with `ast_to_dict` covering all `Expr` variants; schema-version: 1; span on every node

- [`3fcfc5a`] [Quasiquoting](quasiquoting.md) — `quote`: `[quote expr]` special form
  *(Note: this commit is on 2026-05-06 based on its position; exact date not individually verified — falls between ast-dict-core and macro-integration)*

- [`b169f98`] [Canonical AST Dict Schema](ast-schema.md) — `ast-dict-source`: source info and comment metadata in AST dict
  *(Same date range note)*

- [`0344c33`] [Canonical AST Dict Schema](ast-schema.md) — `dict-to-ast`: reverse conversion and `eval-ast` builtin

- [`3fcfc5a`] [Quasiquoting](quasiquoting.md) — `unquote`: `[unquote]` and `[unquote-splice]`

- [`3ca898e`] [tinct-Hosted Formatter](tinct-hosted-formatter.md) — `formatter-compact`: compact formatter modes in tinct; `format_source_tinct()` API

- [`50757b0`] [Desugaring as Macros](macros.md) — `defmacro`: `[defmacro]` + expansion loop infrastructure

- [`79635a1`] [tinct-Hosted Formatter](tinct-hosted-formatter.md) — `formatter-full`: full tinct-hosted pretty formatter

- [`d1e9ebb`] [Desugaring as Macros](macros.md) — `macro-hygiene`: scope sets, working expansion, dual-span provenance

- [`c4306d6`] [Desugaring as Macros](macros.md) — `macro-integration` (completing sprint): include ordering, `$_` port verified, formatter support; macro system complete
  - All macros cluster commits: `0b32540` → `quote` → `unquote` → `ast-dict-source` → `dict-to-ast` → `formatter-compact` → `defmacro` → `formatter-full` → `macro-hygiene` → `c4306d6`

- [`8c745fb`] — `accept typing-cluster`: all 12 typing-cluster whatif proposals formally accepted; implementation sprints written into TODO.md

- [`c3ac42b`] [`let` Binding Form](let-binding.md) — `let-binding`: multi-expression function bodies with sequential scoping; `Expr::Sequential`

## 2026-05-07 — Typing Cluster Implementation + Stdlib

All typing-cluster sprints and the first supplemental stdlib sprints landed on 2026-05-07:

- [`c3ac42b`] [`let` Binding Form](let-binding.md) — *(note: `c3ac42b` author date is 2026-05-06 22:35; DONE.md records it under the typing-cluster sprint sequence)*

The typing-cluster implementation sequence (all 2026-05-07 author dates, with `null-semantics` and `access-pipeline` woven in):

- `18a9d4a` [Pattern Matching](pattern-matching.md) — `pattern-matching-basic`
- `3949dab` [Pattern Matching](pattern-matching.md) — `pattern-matching-destructure`
- `e0e6777` [Union Types](union-types.md) — `union-types`
- `3ef7cc3` [Formal Gradual Typing](gradual-typing.md) — `gradual-typing-split`: `Type::Any` → `Unknown`/`Top`
- `7a16032` [Parameterized Type Aliases](parameterized-type-aliases.md) — `param-type-aliases`
- `96b2437` [Type Classes](typeclasses.md) — `type-classes-constrained`
- `4d98a13` [Path-Sensitive Narrowing](narrowing.md) — `narrowing-basic`
- `0dc913f` [Path-Sensitive Narrowing](narrowing.md) — `narrowing-predicates`
- `8079b67` [Algebraic Data Types](algebraic-data-types.md) — `adts`
- `1c0ced9` [Nominal Variants](nominal-variants.md) — `nominal-variants-unit`
- `2235659` [Nominal Variants](nominal-variants.md) — `nominal-variants-full`
- `cd33072` [Pattern Matching](pattern-matching.md) — `exhaustiveness` (Maranget 2007 usefulness algorithm)
- `54970c1` [Pattern Matching](pattern-matching.md) — `pattern-matching-guards`
- `58c01af` [Type Classes](typeclasses.md) — `type-classes-full`: kind system, class/instance declarations, dictionary passing
- `234a580` *(algebraic-subtyping — Simple-sub intermediate design; fully superseded by BAS 2026-05-09)*
- `499d66a` [Algebraic Data Types](algebraic-data-types.md) — `recursive-adts`
- `fe462a1` [Structural Contracts](structural-contracts.md) — `blame-tracking`
- `4f36628` [Structural Contracts](structural-contracts.md) — `structural-contracts-input`
- `b721db0` [Structural Contracts](structural-contracts.md) — `structural-contracts-validate`
- `7890443` [Structural Contracts](structural-contracts.md) — `structural-contracts-describe`
- [`40f85e4`] [Structural Contracts](structural-contracts.md) — `structural-contracts-blame` (completing sprint)
- `3387092` [Numeric Types](numeric-types.md) — `numeric-range`
- `2ddc5e7` [Numeric Types](numeric-types.md) — `numeric-decimal`
- `6f7e4c0` [Numeric Types](numeric-types.md) — `numeric-bigint`
- [`6fa0c58`] [Numeric Types](numeric-types.md) — `numeric-repr` (completing sprint)

**Note:** `234a580` introduced Simple-sub (Parreaux 2020) as an intermediate design. It was fully superseded by BAS (`bas-core`, 2026-05-09) which replaced it with Boolean-Algebraic Subtyping. See Supersession Map.

Null semantics and access-pipeline also folded into this cluster, their commits dating to 2026-05-05–07.

### Supplemental stdlib sprints (also 2026-05-07)

- [`83c3b51`] [Supplemental Stdlib Modules](lib-supplemental.md) — `string-view`: `Value::String { source: Rc<str>, start, end }` zero-copy representation; `str-slice`, `str-contains?` builtins
- [`e57403e`] [Supplemental Stdlib Modules](lib-supplemental.md) — `string-utils` (completing first round): `starts-with?`, `ends-with?`, `str-chars`; `stdlib/strings.llt` with `pad-left`, `pad-right`, `str-find`, `str-reverse`

(Additional lib-supplemental sprints — math-builtins, bitwise-encoding, bytes-type, toml-lite-path, fscap-protocol, handle-caps — landed across 2026-05-07–08; exact per-sprint dates not individually pinned but confirmed within this window)

## 2026-05-08 — String Interpolation via Macro, Datetime, Regex, TLS Stubs

- [`48ccf3f`] [String Interpolation](string-interpolation.md) — `tmpl-macro`: `i"..."` migrated from parser to `[defmacro tmpl]`; `tmpl-transformer` in `stdlib/macros.llt`; `desugar_interpolated_string()` removed from parser

- [`3cbfd2f`] [Date-Time Support](lib-datetime.md) — `datetime`: `Timestamp`/`Duration`/`ClockCap`/`Timezone` value types; 29 datetime builtins via jiff 0.1; `stdlib/datetime.llt`

- [`a63f8e2`] [Pure-Tinct Regex Engine](lib-regex.md) — `regex` (MVP): pure-tinct regex engine in `stdlib/regex.llt` with literal string matching; full public API (`re-compile`, `re-match`, `re-find`, `re-findall`, `re-replace`, `re-split`); full NFA engine in `net-gaps` sprint

- [`0ab139f`] [TLS, PKI, and HTTP](lib-tls.md) — `connector-tls` (Phase 1): TLS dependencies added; `tls-connect`/`tls-peer-cert` placeholders; `spki-pin` fully functional

- [`07bc1ad`] [TLS, PKI, and HTTP](lib-tls.md) — `connector-tls-full` (completing TLS sprint): full TLS handshake; CA roots (system + Mozilla); mTLS; ALPN; SPKI pinning; `tls-layer` replaces `tls-connect`

## 2026-05-09 — Boolean-Algebraic Subtyping, Error Patterns, Record/Map Split, Networking v2

- [`2035382`–`44005a9`] [Boolean-Algebraic Subtyping](boolean-algebraic-subtyping.md) — `bas-core` (multi-commit series): `Type::Negation`/`Type::Never` foundation; S-RcdTop/S-ClsBot; RDNF groundwork; RowVar removal steps 1–5; corpus updates
  - Commits: `2035382` (Negation/Never) → `fdfaae6` (S-RcdTop/S-ClsBot, RDNF) → `bcc5efb` (C-Var1/2, false-branch narrowing) → `7e71d5b` (annotation fix) → `06a0109` (multi-field annotation as intersection) → `c645cbf`–`a7743cb` (RowVar removal) → `991237f` (RDNF steps 1–3) → `44005a9` (TCO test + corpus warn updates, 2026-05-10)

  *(Note: first bas-core commit is 2026-05-09; last is 2026-05-10 — using first commit's date as the sprint start, last as completing date)*

- [`4d4719b`] [Consistent Error Handling](error-patterns.md) — `result-nominal`: `try` returns `Ok`/`Err` nominal variants; `Result` type + combinators; `stdlib/io.llt` and `stdlib/toml-lite.llt` retrofitted

- [`9282d38`] [Composable Networking v2](lib-net-v2.md) — `connect-v2` (first commit): `tls-layer` replaces `tls-connect`; Handle refactor; `UnixStream`; transport dispatch

- [`fb4baaf`–`9324bd5`] [Record/Map Split and Parameterized Maps](parameterized-dict.md) — `record-map-split`: `Type::Map`; `get?`; `record?`/`map?`; structural dict equality; check_get narrowing for Map

## 2026-05-10 — BAS Complete, Networking v2 Complete, HTTP Sessions

- [`44005a9`] [Boolean-Algebraic Subtyping](boolean-algebraic-subtyping.md) — `bas-core` final commit: TCO test re-enabled; corpus warn sections updated for BAS semantics (`Unknown` displays as `_`)

- [`492a3f9`] [Composable Networking v2](lib-net-v2.md) — `connect-v2` (completing sprint): UDP fully implemented; `UnixDatagram`; `icmp-ping` via `SOCK_DGRAM + IPPROTO_ICMP`
  - Commits: `9282d38` (tls-layer/UnixStream) → `34d5a82` (UDP) → `da18d10` (UnixDatagram) → `492a3f9` (icmp-ping)

- [`1327050`] [Composable Networking v2](lib-net-v2.md) — `http-sessions` (completing sprint): `http2-session` via reqwest ALPN; `stdlib/net.llt` scheme dispatch (https→http2-session); `stdlib-protocols` (grpc.llt, dns.llt, websocket.llt, socks5.llt)
  - Commits: `68f222e` → `9a34a91` → `52f41a8` → `1327050`

## 2026-05-12 — Higher-Kinded Types Foundation

- [`f87cd9c`] [Higher-Kinded Types, Monadic `[do]`, and Precise Field Access](hkt-monads.md) — `hkt-foundation-a`: `Type::App`, `Type::Operator`, Kind system (`Kind::Operator`, `Kind::Label`); UNIFY-OPERATOR, UNIFY-OPERATOR-SYM, UNIFY-APP rules; `Expr::TypeApp` AST node; 16 source files updated

- [`b8a4046`] [Higher-Kinded Types, Monadic `[do]`, and Precise Field Access](hkt-monads.md) — `hkt-foundation-b`: `Label` ADT; `kind_env: HashMap<String, Kind>` in `InferState`; `ClassDecl.superclasses` extended to `Vec<(String, String)>`; `TypeScheme.label_vars`; `check_kind_wellformed()`

- [`5e08f42`] [Higher-Kinded Types, Monadic `[do]`, and Precise Field Access](hkt-monads.md) — `hkt-field-access`: `HasField` constraint (as enum variant); `resolve_has_field()` with depth limit; `check_get` and `check_get_in` extended for label TypeVars

- [`6c5874c`] [Higher-Kinded Types, Monadic `[do]`, and Precise Field Access](hkt-monads.md) — `hkt-bas`: BAS `App` subtyping verified; 4 unit tests; covariant `App(f,a) <: App(f,b)` confirmed; reverse direction correctly rejected

- [`c96981b`] [Higher-Kinded Types, Monadic `[do]`, and Precise Field Access](hkt-monads.md) — `hkt-kind-inference`: E091 kind mismatch error code; `TypeError::kind_mismatch()` helper; kind pre-pass, App inference, normalization; `[do]` macro; `sequence`/`traverse`; full Functor/Applicative/Monad/Foldable/Traversable hierarchy

---

## Features Without a Dedicated Sprint Date

The following feature docs correspond to design documents or grouping docs that do not have an independent implementation sprint:

- [Null Semantics](null-semantics.md) — implemented as part of the typing cluster (2026-05-07); `@Null` = `Type::Record(Row::Empty)`; no standalone sprint
- **macro-rewrite** — fully superseded; doc deleted. Features delivered via `Expr::Sequential` (let-binding), `Expr::Match` (pattern-matching), and `[defmacro tmpl]` (string-interpolation).
- **macros-cluster** — planning/coordination doc; deleted. Constituent features each have their own doc: [ast-schema.md](ast-schema.md), [quasiquoting.md](quasiquoting.md), [macros.md](macros.md), [tinct-hosted-formatter.md](tinct-hosted-formatter.md).

---

## Supersession Map

### Boolean-Algebraic Subtyping → Earlier Type System Features

[boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09) changed the following features implemented in the typing cluster (2026-05-07):

| Feature Doc | What Changed |
|-------------|-------------|
| [union-types.md](union-types.md) | Simple-sub (Parreaux 2020) replaced by BAS; `Type::Any` replaced by `Unknown`/`Top`; `unify` retained alongside BAS `constrain()`; S-RcdTop collapses disjoint-key single-field record unions to Top |
| [algebraic-data-types.md](algebraic-data-types.md) | S-RcdTop makes structural discriminated unions (`{ok:T}\|{err:S}`) invalid as ADTs; use nominal variants instead |
| [typeclasses.md](typeclasses.md) | `TypeScheme.row_vars` removed; `constraints` and `label_vars` fields added |
| [gradual-typing.md](gradual-typing.md) | `Type::Any` split into `Unknown` (gradual opt-out) and `Top` (supertype) |
| [narrowing.md](narrowing.md) | False-branch narrowing now implemented via `Negation(T)` (not deferred); `apply_negation_narrowings` in `src/typecheck.rs` |
| [parameterized-type-aliases.md](parameterized-type-aliases.md) | `TypeScheme.row_vars` removed; all records closed under BAS |
| [nominal-variants.md](nominal-variants.md) | Nominal variants are now required (not optional) for discriminated unions, because S-RcdTop makes structural record unions collapse to Top |

### error-patterns → Earlier Error/Result Semantics

[error-patterns.md](error-patterns.md) (2026-05-09):

| Feature Doc | What Changed |
|-------------|-------------|
| [union-types.md](union-types.md) | `try` returns `Value::Variant { tycon: "Result", ctor: "Ok"/"Err", .. }` not structural `{ok: v}`/`{err: msg}` |
| [algebraic-data-types.md](algebraic-data-types.md) | Same — `try` result type changed from structural ADT to nominal Result |

### parameterized-dict → Earlier Dict/Record Semantics

[parameterized-dict.md](parameterized-dict.md) (2026-05-09):

| Feature Doc | What Changed |
|-------------|-------------|
| [null-semantics.md](null-semantics.md) | `@Dict` now resolves as closed empty Record, not the old open-record semantics |
| [algebraic-data-types.md](algebraic-data-types.md) | Dict/Map split — `@Record` no longer implies open record with RowVar tail |

### hkt-monads → typeclasses

[hkt-monads.md](hkt-monads.md) (accepted 2026-05-11):

| Feature Doc | What Changed |
|-------------|-------------|
| [typeclasses.md](typeclasses.md) | Phase 2 hierarchy (Functor/Applicative/Monad/Foldable/Traversable) is specified in hkt-monads, not typeclasses; `Kind::Operator` added; `TypeScheme.label_vars` added |

### macro-rewrite → Superseded (doc deleted)

The macro-rewrite proposal was superseded before implementation. Its features were delivered via direct Rust AST variants instead of macros:

- `let` binding → `Expr::Sequential` (see [let-binding.md](let-binding.md))
- `match` → `Expr::Match` + `SurfaceMatchArm.pattern: Arc<SurfaceNode>` (see [pattern-matching.md](pattern-matching.md))
- `i"..."` → `[defmacro tmpl]` via `tmpl-transformer` (see [string-interpolation.md](string-interpolation.md))

The feature doc was deleted as fully superseded.
