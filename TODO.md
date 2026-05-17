# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Type Stage Features

*All sprints below depend on `type-stage-infra`.*

### chr-unification: CHR-unified type constraints — FDs and type families

See `doc/whatif/chr-unification.md`. **State: Proposal** — design not yet approved; sprint tasks to be written after /rnd approval. `type-stage-infra` is the required groundwork (type dict schema + `type_to_dict`/`dict_to_type` are the FFI between inference and type-stage resolvers).

**Depends on:** `type-stage-infra`, `typeclass-mptc-fundeps`

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

- [x] Migrate `identity`, `const`: consolidate `name@[doc: "..."]` + bare `fn` into `fn@[return: a  doc: "..."]`; drop comment block (`stdlib/prelude.llt`)
- [x] Migrate `not`: `fn@[return: Bool  doc: "Boolean negation.\n\nExample: [not true] => false"]`; drop comment block (`stdlib/prelude.llt`)
- [x] Migrate `and`, `or`: full metadata dict with `doc:` including `# NOTE:` content about lazy evaluation semantics; drop comment blocks (`stdlib/prelude.llt`)
- [x] Migrate `any?`, `all?`: add `doc:` with type, examples, and materialization note (`stdlib/prelude.llt`)
- [x] Migrate `>`, `<=`, `>=`: added `doc:` to existing `fn@[return: Bool constraint: [a: Comparable]]` (`stdlib/prelude.llt`)
- [x] Migrate `quot`, `mod`: `fn@[return: Int/Number  doc: "..."]`; includes semantics notes (`stdlib/prelude.llt`)
- [x] Migrate `ceil`, `trunc`, `abs`, `sign`, `clamp`: `fn@[return: T  doc: "..."]`; includes examples (`stdlib/prelude.llt`)
- [x] Migrate `min`, `max`: added `doc:` to existing `fn@[return: a  constraint: [a: Comparable]]` (`stdlib/prelude.llt`)
- [x] Migrate `sum`, `product`: `fn@[return: Number  doc: "..."]`; includes empty-collection base-case notes (`stdlib/prelude.llt`)
- [x] Migrate `between?`: full metadata dict with examples — `average`, `gcd`, `lcm` do not exist in prelude (skipped) (`stdlib/prelude.llt`)
- [x] Verify `just test-lib` passes; all 2185 tests pass; 6 corpus test expectations updated (`stdlib/prelude.llt`)

### prelude-annotations-b: Collection and Dict operations

Public functions in prelude.llt, collection section. ~25 functions:
`length`, `keys`, `values`, `entries`, `has?`, `get`, `get-or`, `get?`, `get-in`, `get-in-or`, `remove`, `remove-keys`, `keep-keys`, `reindex`, `merge-with`, `group-by`, `frequencies`, `index-by`, `map-entries`, `map-keys`, `map-values`, `flat-map`, `zip`, `unzip`, `partition`, `take-while`, `drop-while`, `sliding`, `chunks`.

- [x] Migrate `get`, `get-or`, `get?`: `@Label` preserved on key param; `fn@[return: a  doc: "..."]` with HasField constraint; key-not-found behavior note (`stdlib/prelude.llt`)
- [x] Migrate `get-in`, `get-in-or`: doc includes path-traversal semantics and Null-propagation note (`stdlib/prelude.llt`)
- [x] Migrate `has?`, `remove`, `remove-keys`, `keep-keys`, `values`, `entries`, `from-entries`: full metadata dict with examples (`stdlib/prelude.llt`)
- [x] Migrate `reindex`, `group-by`, `deep-merge`, `transpose`, `flatten`: full metadata dict with examples; O(n²) notes preserved (`stdlib/prelude.llt`)
- [x] Migrate `map-entries`, `flat-map`, `zip`, `unzip`, `partition`, `take-while`, `drop-while`: full metadata dict with examples (`stdlib/prelude.llt`)
- [x] Migrate `slice`, `find-deep`, `with-entries`, `walk`: full metadata dict (`stdlib/prelude.llt`)
- [x] Verify `just test-lib` passes; all 2185 tests pass; line number expectations updated in 2 corpus tests (`stdlib/prelude.llt`)

### prelude-annotations-c: Sequences, Strings, Control flow, Error handling

Public functions: `range`, `repeat`, `iterate`, `cycle` (seq); `str-join`, `str-split`, `str-trim`, `str-pad-left`, `str-pad-right`, `str-replace`, `str-find`, `str-reverse`, `format`, `parse-int`, `parse-float` (string); `if`, `cond`, `when`, `unless`, `try`, `error`, `assert` (control); `->`, `|>`, `compose`, `flip`, `partial` (combinators).

- [x] Migrate sequence generators/ops: range, repeat, iterate, cycle, seq, head, tail, collect, unfold, join, concat, first, last, rest, cons, reverse, sort (`stdlib/prelude.llt`)
- [x] Migrate control flow: cond, when, unless (`stdlib/prelude.llt`)
- [x] Migrate error handling: try-or, assert (`stdlib/prelude.llt`)
- [x] Migrate combinators: ->, compose (`stdlib/prelude.llt`)
- [x] Verify `just test-lib` passes; all 2185 tests pass (`stdlib/prelude.llt`)

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
