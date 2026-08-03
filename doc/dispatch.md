# Unified Dispatch Design

This chapter documents the unified dispatch mechanism that powers all callable invocations in tinct: ordinary functions, pattern-match arms, and typeclass instance methods.

---

## 1. CoreClause: The Unified Callable Unit

Every callable entity — whether an ordinary function, a match arm, or an typeclass instance method — is represented at CoreExpr level as one or more `CoreClause` structures. A `CoreClause` bundles:

- **params**: parameter names and annotations
- **lowered_pattern**: structural pattern (for match arms)
- **guard**: optional guard expression
- **body**: the expression to evaluate when the clause matches
- **captures**: closure environment bindings

An ordinary function is a single wildcard clause (no pattern, no guard). A match expression is a list of clauses with structural patterns. An instance method body is a clause attached to an instance declaration.

---

## 2. try_clause: Unified Evaluation Order

The `try_clause` function in `src/eval_call.rs` implements the common dispatch logic for all clauses. Evaluation proceeds in strict order:

1. **Arity check** — verify positional argument count matches required params
2. **Type guards** — if params have type annotations (`@Integer`, `@String`), materialize arguments and check runtime types via `value_matches_type`
3. **Structural pattern** — if `lowered_pattern` is present, test whether the scrutinee matches the pattern structure (dict keys, variant tags, nested patterns)
4. **Guard expression** — if `guard` is present, evaluate it in the clause's environment and test for truthiness
5. **Bind and evaluate body** — if all checks pass, bind parameters to arguments, bind pattern variables, and evaluate the body

This order is fixed and applies uniformly to all dispatch forms. There are no special cases.

---

## 3. VarAddr: Lexical vs Effect Dispatch

Variable resolution at call time uses `VarAddr` to distinguish between two dispatch modes:

### VarAddr::Dispatch { depth, slot }

Standard lexical lookup. The name resolves to a specific binding at a known de Bruijn depth and slot in the environment chain. Used for:

- Ordinary function calls
- Let-bound variables
- Closure captures
- Dict entries

This is the common case. The resolver determines the binding site at resolution time; the evaluator retrieves it at call time.

### VarAddr::EffectPerform { class_id, method }

Open dispatch via accumulated instance group scan. The name is a typeclass method (e.g., `+`, `=`, `<`). At call time, the evaluator uses a two-phase **collect-then-select** strategy:

**Phase 1 — Collect:** Collect all `Value::Function` entries whose `instance_of` matches `(class_id, method)` as candidates (innermost-first). Each candidate preserves its own `closure_env`. If no candidates are found, raise a "no instance" error.

**Phase 2 — Select:** Iterate candidates; for each, call `invoke_function` which tries each clause via `try_clause`. The first candidate whose clauses match is executed. Type discrimination happens here — Step 2 of `try_clause` materializes arguments and checks runtime types via `value_matches_type` against each clause's parameter annotations. The first clause whose type guards, pattern, and guard all pass is executed.

Used for:

- Typeclass method calls (`+`, `-`, `*`, `/`, `=`, `<`, `>`, etc.)
- User-defined typeclass instances

The instance group is lexically scoped — inner dicts shadow outer dicts, enabling local instance overrides.

---

## 4. Three Forms of Dispatch

### Ordinary Functions

An ordinary function is a `CoreExpr::Fn` with a single clause. The clause has:

- `params`: the function's parameter list
- `lowered_pattern`: `None` (no pattern match)
- `guard`: `None` (no guard)
- `body`: the function body expression
- `captures`: closure environment

Calling an ordinary function materializes the function value, checks arity, binds arguments to params, and evaluates the body. No pattern matching, no type dispatch.

### Match Arms (Closed Dispatch)

A match expression is a `CoreExpr::Match` with a scrutinee and a list of `CoreMatchArm` entries. Each arm is a clause with:

- `params`: for **case arms** (`[case [let bindings] pattern body]`), populated with binding names as `CoreParam` entries (one per `[let ...]` name, slot-indexed); for **keyed arms** (literal, variable, or wildcard patterns without `[let ...]`), empty
- `lowered_pattern`: the structural pattern (dict keys, variant tags, nested patterns)
- `guard`: optional guard expression
- `body`: the arm body
- `captures`: pattern bindings

Evaluation tries each arm in order via `try_clause`. The first arm whose pattern and guard both succeed is executed. If no arm matches, a "non-exhaustive match" error is raised.

This is **closed dispatch**: the set of arms is fixed at the match site. Adding a new variant type does not extend existing match expressions.

### Instance Methods (Open Dispatch)

A typeclass instance method is a clause attached to an instance declaration. The clause has:

- `params`: the method's parameter list (from the instance arm pattern)
- `lowered_pattern`: type annotations from the instance arm (e.g., `[let a@Integer b@Integer]`)
- `guard`: `None` (guards are not supported in instance arms)
- `body`: the method implementation
- `captures`: lexical captures from the instance's scope

At resolution time, instance methods are registered in the accumulated instance group with a key `(class_id, method_name, type_args)`. At call time, `VarAddr::EffectPerform` scans the group for a matching entry, retrieves its clause, and evaluates it via `try_clause`.

This is **open dispatch**: new instances can be added in any scope, and inner scopes shadow outer scopes. The set of instances visible at a call site depends on the lexical environment, not a global registry.

---

## 5. class_decl_id and type_decl_id: Stable Numeric Identity

Every `[class ...]` declaration is assigned a unique `class_decl_id` (u64) at resolution time via an atomic counter. Every `[type ...]` declaration is assigned a unique `type_decl_id` (u64) at lowering time.

These numeric IDs provide stable identity that does not depend on string matching:

- **class_decl_id**: used as the first component of instance group keys. Two classes with the same name in different scopes get different IDs, preventing collision.
- **type_decl_id**: used to stamp variant values with their type identity. `match_pattern` uses `Arc::ptr_eq(variant.type_val, dict.type_val)` to test type membership without string comparison.

Numeric IDs are never serialized or persisted. They exist only during a single evaluation run. This design prevents cross-run identity confusion and ensures that type identity is always derived from the lowering/resolution context, not from user-provided strings.

---

## 6. Why the Synthetic MethodDispatcher Was Removed

Prior to this design, typeclass method dispatch was implemented via a **synthetic MethodDispatcher function** emitted by the lowerer. For each method name (`+`, `=`, etc.), the lowerer generated a `CoreExpr::Fn` that:

1. Matched the first argument (`__d0`) against type-name patterns (`Integer`, `String`, `Float`)
2. Called the corresponding mangled instance binding (`ɪɴꜱᴛᴀɴᴄᴇ⧼Addable∷+⟨Integer⟩⧽`)
3. Fell back to a parent dispatcher (for cross-dict composition) or raised "no instance"

This approach had several problems:

### Parallel Mechanism

MethodDispatcher was a parallel implementation of dispatch logic. It duplicated pattern matching, type checking, and error handling — all of which already existed in `try_clause` and `match_pattern`. Two implementations mean two sets of bugs, two maintenance burdens, and divergent behavior.

### String-Based Type Dispatch

MethodDispatcher patterns were VarRef nodes resolved to type bindings (e.g., `"Integer"`, `"String"`). At runtime, the match evaluator called `typenode_ctor_to_typevalue` to convert the type name string to a `TypeValue`, then called `value_matches_type` to test the scrutinee's type. This introduced a string-matching layer that was unnecessary and fragile.

With `VarAddr::EffectPerform`, type dispatch uses the argument's **runtime `Value` variant** directly. No string conversion, no TypeNode lookup, no string matching. The instance group key already encodes the type identity.

### Synthetic Code Generation Complexity

The lowerer's `make_method_dispatcher_fn` function was ~700 lines of code that synthesized:

- Parameter lists (`__d0`, `__d1`, ...)
- Type-name patterns (VarRef nodes with ClosureCapture addresses)
- Mangled binding calls (forwarding args through closure captures)
- Diagnostic dict construction (dynamic `[builtin-string-concat prefix [builtin-llt-repr [builtin-type-of __dN]]]` calls)
- Parent dispatcher chaining (scanning scope_frames for "second occurrence" of the method name)
- Wildcard fallback arms (raise "no instance" with parameter type notes)

All of this complexity is eliminated by `VarAddr::EffectPerform`. The evaluator's native instance group scan replaces the synthetic dispatcher, and the natural `try_clause` path handles all type checking and error reporting.

### Scope Chaining Fragility

MethodDispatcher parent chaining worked by capturing the method name from an ancestor scope frame (the "second occurrence" algorithm). This required:

- Trimming scope_frames to exclude inner dict frames (to avoid self-referential captures)
- Identifying the "current dict" by finding the innermost frame containing all mangled instance names
- Forwarding calls to the parent dispatcher when no local instance matched

This logic was correct but fragile. It depended on precise frame ordering, unique mangled names, and careful handling of nested dict scopes. With `VarAddr::EffectPerform`, scope chaining is implicit: the instance group is built during letrec evaluation, and inner instances naturally shadow outer instances via the environment chain.

### Diagnostic Quality

When MethodDispatcher raised "no instance", it synthesized a Diagnostic dict with dynamic type notes (e.g., `"matched parameter 0: Integer"`, `"no match for parameter 1: String"`). This required calling `builtin-type-of`, `builtin-llt-repr`, and `builtin-string-concat` at runtime to construct the notes.

With `VarAddr::EffectPerform`, the evaluator can directly inspect the argument values and emit a structured error with type information. No synthetic calls, no string concatenation, no reliance on builtins being in scope.

---

## 7. The Unified Path Forward

The unified dispatch design eliminates all synthetic code generation and all parallel mechanisms. There is one dispatch path for all callables:

1. Resolve the name to a `VarAddr` (either `Dispatch` for lexical lookup or `EffectPerform` for open dispatch)
2. Materialize the function value (for `Dispatch`) or scan the instance group (for `EffectPerform`)
3. Retrieve the clause(s)
4. For each clause, call `try_clause` (arity → type guards → pattern → guard → bind → eval)
5. Return the first successful result or raise "no match" / "no instance"

This is the same path for ordinary functions, match arms, and instance methods. The only difference is how the clause is located (`VarAddr` determines the lookup mechanism). Once located, evaluation is uniform.

---

## 8. Implementation Status

**Current state (completed in S-1024):**

- `VarAddr::Dispatch` and `VarAddr::EffectPerform` are defined in `src/ast.rs`
- `try_clause` handles arity, type guards, patterns, guards, and body evaluation; structural pattern bindings are correctly populated into the call frame
- `class_decl_id` and `type_decl_id` are assigned at resolution/lowering time; `type_decl_id` is carried directly on `Value::Variant` for O(1) type identity checks
- `MethodDispatcher` synthesis has been fully removed from `src/lower.rs`; instance methods emit `instance_of: Some((class_decl_id, method))` on their `CoreExpr::Fn`
- `VarAddr::EffectPerform` is emitted by the resolver for typeclass method references both at document scope and inside function bodies
- The evaluator's `CoreExpr::Var` arm scans the accumulated group for matching instance functions and synthesizes a dispatch function on demand
- `resolve_surface_document_with_seed_frames_and_classes` and `resolve_surface_program_with_classes` expose the class-aware resolver variants at all call sites
- Corpus tests for dispatch errors verify the correct error substrings for both typed parameter mismatches and missing instances

**Remaining work:**

None. All dispatch tasks are resolved. (T-2141 per-clause closure environments was resolved by T-2146 EffectPerformDispatcher — each candidate now preserves its own `closure_env`.)

---

## References

- Wadler, P. & Blott, S. (1989). How to make ad-hoc polymorphism less ad hoc. POPL '89, pp. 60-76. — Dictionary-passing style (the inspiration for instance group accumulation).
- Kiselyov, O., Sabry, A., & Swords, C. (2013). Extensible effects: an alternative to monad transformers. Haskell '13, pp. 59-70. — Effect handlers as open dispatch.
- Leijen, D. (2017). Type directed compilation of row-typed algebraic effects. POPL '17, pp. 486-499. — Evidence passing for row-typed effects (analogue to our instance group keys).
- Lindley, S., McBride, C., & McLaughlin, C. (2017). Do be do be do. POPL '17, pp. 500-514. — Frank's "ports" (anti-closures) as a model for effect dispatch.

See also:
- `doc/16b-rust-tinct-protocol.md` §7 (Protocol Entry Points) for the `builtin-raise` protocol
- `doc/05-types.md` (Type System Overview) for class and type declarations
- `doc/06-type-inference.md` (Type Inference and Checking) for typeclass resolution
- `doc/feature/typeclass.md` (Typeclass Design) for the user-facing typeclass model
