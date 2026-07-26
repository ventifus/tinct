# Case Arm Redesign

**Status:** Implemented (Sprint S-994)  
**Related:** S-869 (case-let-unified), doc/07-pattern-matching.md

## Problem

Prior to S-994, `SurfaceExpression::CaseArm` and `CoreExpr::CaseArm` existed as expression variants, but they are not expressions — they are arm-level constructs. The parser stored `[case [let v] [ok: v] body]` arms using a **sentinel hack**:

- `SurfaceMatchArm.pattern` held a `Placeholder` sentinel
- `SurfaceMatchArm.body[0]` held the real `CaseArm` node
- Every downstream consumer (resolver, type checker, lowerer, evaluator) had to know this convention and extract the real data from inside the sentinel

This caused:
1. **Coverage checker false positives**: every `[case ...]` arm was classified as Wildcard because `ast_pattern_to_coverage` only saw the `Placeholder` sentinel
2. **7 manual extraction sites** across the codebase, all duplicating the same `LetDecl` extraction logic
3. **Coupling to implementation detail**: every downstream consumer had to know the sentinel convention

## Solution

Sprint S-994 removed the sentinel and the `CaseArm` expression variants entirely. Instead:

- **`SurfaceMatchArm.let_bindings: Option<Arc<SurfaceNode>>`** — stores the `[let v w ...]` binding list directly
- **`CoreMatchArm.lowered_pattern: Option<Arc<Spanned<CoreExpr>>>`** — stores the lowered CoreExpr version of the pattern for evaluator use

When `let_bindings` is `Some(...)`, the arm is a case arm. When it's `None`, the arm is a keyed arm (pattern-only, no binding variables).

## AST Changes

### SurfaceMatchArm
```rust
pub struct SurfaceMatchArm {
    pub pattern: Arc<SurfaceNode>,
    pub let_bindings: Option<Arc<SurfaceNode>>,  // NEW: [let v w ...] or None for keyed arms
    pub guard: Option<Arc<SurfaceNode>>,
    pub body: Vec<Arc<SurfaceNode>>,
    pub guard_matchable_binding: MatchableBinding,
}
```

### CoreMatchArm
```rust
pub struct CoreMatchArm {
    pub pattern: Arc<Spanned<CoreExpr>>,
    pub let_bindings: Option<Arc<Spanned<CoreExpr>>>,      // NEW: lowered [let ...] node
    pub lowered_pattern: Option<Arc<Spanned<CoreExpr>>>,   // NEW: lowered pattern for evaluator
    pub guard: Option<Arc<Spanned<CoreExpr>>>,
    pub body: Vec<Arc<Spanned<CoreExpr>>>,
    pub guard_matchable_binding: MatchableBinding,
}
```

## Pattern Dispatch Rule

The evaluator determines whether a case arm pattern is structural or guard-based using `is_structural_pattern_head`:

- **Uppercase or dot-access head** → structural match (constructor pattern)
- **Lowercase or operator head** → guard expression (boolean predicate)
- **Literal** → exact-value match (treated as structural)
- **Placeholder (`...`)** → wildcard (treated as structural)

Structural patterns are matched using `eval_case_arm_structural_pattern`. Guard patterns are evaluated as thunks and checked for truthiness.

## Binding-Var Special Case

`[case [let v] v body]` — the pattern `v` is a `VarRef` that resolves to `Parameter(0)`, the binding variable just introduced. This is **not a guard** — it's an unconditional identity binding.

The evaluator detects this case: when `!is_structural` and the lowered pattern is `CoreExpr::Var { addr: VarAddr::Parameter(_), ... }`, it matches unconditionally without guard evaluation.

**Why:** evaluating it as a guard would require calling the `Matchable` instance's `match?` method on the scrutinee itself, which is circular and semantically incorrect.

## Coverage Fix

`VarAddr::Parameter` patterns are treated as `CoveragePattern::Wildcard` — they are binders, not pins.

**Opaque arms for resolver errors**: Arms with unresolved `VarRef` nodes (`resolution.get() == Some(None)`) are marked opaque (`has_guards = true`). This prevents resolver errors in patterns from masking non-exhaustiveness — an arm with an undefined variable neither contributes to coverage nor is flagged as redundant.

Example:
```tinct
[match 42
  undefined_var: "body"    # resolver error: undefined_var not in scope
  ...:           "fallback"]
```
Without the opaque-arm fix, the match would not be flagged as non-exhaustive because `undefined_var` would be treated as a wildcard. With the fix, the first arm is opaque, and the `...` arm is recognized as necessary.

## Files Changed

- `src/ast.rs` — added `let_bindings` and `lowered_pattern` fields, removed `CaseArm` variants
- `src/parser.rs` — emit `SurfaceMatchArm` directly, populate `let_bindings`, removed sentinel
- `src/resolve.rs` — drive scoping from `arm.let_bindings`, removed CaseArm branch
- `src/lower.rs` — populate `let_bindings` and `lowered_pattern`, removed CaseArm branch
- `src/typecheck_cek.rs` — drive from `arm.let_bindings`, removed CaseArm branch, added opaque-arm handling
- `src/eval_materialize.rs` — use `lowered_pattern` for case arms, added binding-var special case
- `src/coverage.rs` — `VarAddr::Parameter` → `Wildcard`
- `src/desugar.rs`, `src/surface_fmt.rs`, `src/surface_fields.rs`, `src/surface_convert.rs`, `src/eval_core.rs` — removed CaseArm branches or updated construction sites

## Testing

Corpus tests:
- `tests/corpus/eval/match/case_arm_guard_expression.llt-eval` — guard expression pattern (`[case [let x] [> x 0] body]`); multi-binding guard (`[case [let a b] [= a b] body]`)
- `tests/corpus/eval/match/case_arm_binding_var_pattern.llt-eval` — binding-var pattern (`[case [let v] v body]`); laziness proof (unused binding not forced)
- `tests/corpus/eval/match/case_arm_dot_access_pattern.llt-eval` — dot-access constructor pattern (`[case [let p] [Result.Ok p] body]`)
- `tests/corpus/eval/match/case_arm_opaque_arm.llt-eval` — opaque arm: undefined variable in pattern does not silently match; fallback fires
- `tests/corpus/eval/match/case_arm_binding.llt-eval` — structural binding (`[case [let v] v body]` structural form)

## Design Rationale

1. **No sentinel**: the parser constructs the correct AST shape directly. No downstream consumer needs to know about a sentinel convention.
2. **Explicit `let_bindings` field**: the data lives where it's used. Resolver, type checker, and lowerer all access `arm.let_bindings` directly.
3. **Lowered pattern for evaluator**: `CoreMatchArm.lowered_pattern` avoids re-lowering the pattern at eval time and provides the correct lowered CoreExpr for guard dispatch.
4. **Single extraction helper**: `extract_case_arm_binding_names` (in `resolve.rs` and `typecheck_cek.rs`) consolidates the wildcard-exclusion logic into a single point of maintenance.

## References

- Sprint S-994: case-arm-sentinel-removal (8 tasks, T-1907–T-1914)
- Sprint S-869: case-let-unified (introduced `[case [let v] pattern body]` syntax)
- `doc/07-pattern-matching.md` — user-facing pattern matching documentation
- `eval_materialize.rs:is_structural_pattern_head` — dispatch rule implementation
