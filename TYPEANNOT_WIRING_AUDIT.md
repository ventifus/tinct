# TypeAnnotationTable Wiring and Code Audit — 2026-05-23

**Scope:** Four tasks related to TypeAnnotationTable threading, boundary_guards propagation, and error frame filtering.

## Task 1: Wire TypeAnnotationTable in builtin_load and builtin_expand ✅ FIXED

### Problem
`builtin_load` (src/builtins_meta.rs:1629) and `builtin_expand` (src/builtins_meta.rs:1740) parsed, desugared, and resolved files but passed empty `TypeAnnotationTable::new()` to Value::Program. This caused all TypeAssert nodes in loaded/expanded files to be lowered to `CoreExpr::RuntimeTypeCheck` instead of `CoreExpr::TypeAssert`, forcing nominal fallback validation instead of using statically-resolved types.

### Fix Applied
Added `typecheck_surface_program_annotation_table()` call after resolve, before wrapping as Value::Program:

**builtin_load** (src/builtins_meta.rs:1721-1724):
```rust
// Typecheck to populate TypeAnnotationTable for static type resolution in TypeAssert nodes.
// This enables included files to use the resolved type path instead of RuntimeTypeCheck fallback.
let (_annotation_errors, type_annotation_table) =
    crate::typecheck::typecheck_surface_program_annotation_table(&program);
```

**builtin_expand** (src/builtins_meta.rs:1783-1785):
```rust
// Typecheck to populate TypeAnnotationTable for static type resolution in TypeAssert nodes.
let (_annotation_errors, type_annotation_table) =
    crate::typecheck::typecheck_surface_program_annotation_table(&new_surface_program);
```

### Impact
Loaded and expanded files now get statically-resolved types. TypeAssert nodes will use the resolved type path (value_matches_type) instead of falling back to nominal string comparison. This fixes the phase consistency issue where `[@Fn ...]` succeeds with typechecking but fails via $load.

### Cross-Layer Integration
Pipeline order preserved: parse → expand → desugar → resolve → **typecheck** → wrap as Value::Program. Matches the pattern in lib.rs (eval_source_with_config:247-249), main.rs (1945-1946), and repl.rs (225-226).

---

## Task 2: Audit boundary_guards Propagation ✅ CLEAN

### Audit Scope
Check that no fresh EvalContext is created AFTER typechecking in main.rs, repl.rs, and lsp/document.rs. The pattern should be: create context → typecheck (populates boundary_guards) → eval (consumes guards).

### Findings

**main.rs (lines 1920-1942):**
```rust
// Create EvalContext
let eval_ctx = tinct::eval::EvalContext::new_sharing_arena(...);

// Wire boundary guards and do-infer resolutions from type inference
eval_ctx.set_boundary_guards(infer_state.boundary_guards);
eval_ctx.set_do_infer_resolutions(infer_state.do_infer_resolutions);

// Typecheck
let (_annotation_errors, type_annotation_table) =
    tinct::typecheck::typecheck_surface_program_annotation_table(&program);

// Eval (uses the same context)
let file_result = tinct::async_rt::block_on(tinct::eval_surface_file_with_input(...));
```
**Pattern:** Context created → guards set → typecheck → eval. ✅ Correct.

**repl.rs (lines 149-228):**
```rust
// Create EvalContext once at startup (line 154)
let ctx = crate::eval::EvalContext::new_sharing_arena(...);

// Store in ReplSession
self.ctx = ctx;

// Per-expression: typecheck (line 226), then eval (line 228)
let (_annotation_errors, type_annotation_table) =
    crate::typecheck::typecheck_surface_program_annotation_table(&program);
let result_thunk = crate::async_rt::block_on_anywhere(eval_surface_file_with_input(
    &program, ..., &self.ctx, ...
));
```
**Pattern:** Context created once at session initialization. Reused for all expressions. No boundary_guards wiring (REPL doesn't use type inference). ✅ Correct.

**lsp/document.rs (lines 497-558):**
```rust
// Create base_eval_ctx at DocumentStore initialization (line 497)
let base_eval_ctx = crate::eval::EvalContext::new_sharing_arena(...);

// Per-document: typecheck only (line 160), NO eval
let (errs, mut map, docs, smap, tc_diagnostics) =
    crate::typecheck::typecheck_surface_program(prog, seeded_env);
```
**Pattern:** Context created for the store. Documents only typecheck (for hover/diagnostics), never eval. No boundary_guards wiring needed. ✅ Correct.

### Verdict
All three paths preserve boundary_guards correctly. No fresh context created after typechecking. Task complete.

---

## Task 3: Wire TypeAnnotationTable Lookup in force_step ✅ ALREADY IMPLEMENTED

### Expected Work
Check if `types.get(node_id)` is called in the TypeAssert force path. Use resolved type if present instead of RuntimeTypeCheck fallback.

### Finding
**This is already implemented at the lowering stage, not the force_step stage.**

**src/lower.rs:148-164 (lowering SurfaceExpression::TypeAssert):**
```rust
let id = node_id(arc);
match types.get(&id) {
    Some(ty) => CoreExpr::TypeAssert {
        annotation: annotation.clone(),
        expr: Arc::new(lower(inner, res, types)),
        resolved_type: ty.clone(),
    },
    None => {
        // Macro-synthesized node — bypassed typechecking.
        // Use RuntimeTypeCheck for best-effort dynamic validation.
        CoreExpr::RuntimeTypeCheck {
            annotation: annotation.clone(),
            expr: Arc::new(lower(inner, res, types)),
            default: None,
        }
    }
}
```

**src/eval_materialize.rs:1057-1104 (force_step handling CoreExpr::TypeAssert):**
```rust
if let crate::ast::CoreExpr::TypeAssert {
    annotation,
    expr: inner,
    resolved_type,
} = &core_expr.node
{
    // ... push Cont::TypeAssertCheck with resolved_type
    stack.push(Cont::TypeAssertCheck(Box::new(TypeAssertCheckData {
        annotation: Box::new(annotation.clone()),
        resolved: Box::new(Some(resolved_type.clone())),  // <-- resolved type passed through
        // ...
    })));
}
```

**src/eval_materialize.rs:2251-2356 (apply_cont Cont::TypeAssertCheck):**
```rust
match result {
    Ok(value) => match *resolved {
        Some(expected) => {
            // Use resolved type from type checker
            if let Some(row) = as_record_row_merged(&expected) {
                // Structural record validation
            } else if value_matches_type(&value, &expected) {
                // Tag-only validation via resolved type
            }
        }
        None => {
            // --no-typecheck FALLBACK (nominal validation)
            // String comparison of type names
        }
    }
}
```

### Verdict
TypeAnnotationTable lookup happens at lowering (src/lower.rs:149). If a resolved type exists, `CoreExpr::TypeAssert` is created. If not, `CoreExpr::RuntimeTypeCheck` is created. Force_step correctly uses the resolved type when present. **No additional work needed.**

The TODO item description ("TypeAssert still always uses RuntimeTypeCheck fallback") was incorrect — the resolved type path is fully wired. The issue was that `builtin_load` and `builtin_expand` passed empty TypeAnnotationTable, causing ALL nodes in loaded files to hit the `None` case at lowering. Task 1 fixed this.

---

## Task 8: Verify Span::origin() Frame Filtering ✅ ALREADY IMPLEMENTED

### Requirement
Check if `EvalError::Display` filters out frames with `Span::origin()` (0:0-0:0 synthetic spans). Stdlib calls should not add "in <function> at 0:0-0:0" noise to error traces.

### Finding

**src/error.rs:1632-1641 (should_display_frame helper):**
```rust
/// Returns `true` if the stack frame should appear in user-facing error output.
/// Returns `false` only for synthetic origin spans (Span::origin() = offset 0, line 1, col 1)
/// from stdlib/builtin calls — these have no meaningful source location.
///
/// NOTE: No suffix-based filtering is applied. Every frame with a real source location
/// is shown, including stdlib internal helpers (-impl, -step, -check, -merge). This is
/// necessary for diagnosing bugs in macro transformers and stdlib code.
fn should_display_frame(frame: &StackFrame) -> bool {
    frame.span != Span::origin()
}
```

**src/error.rs:1707-1863 (EvalError::Display implementation):**
```rust
impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // ... format definition span and kind ...

        // Stack trace
        for frame in &self.stack {
            if !should_display_frame(frame) {  // <-- filter applied
                continue;
            }
            writeln!(f, "  in {} at {}", frame.label, frame.span)?;
        }
        // ...
    }
}
```

**Test coverage (src/error.rs:3261-3295):**
```rust
#[test]
fn test_origin_span_frames_filtered_from_display() {
    // Verify that stack frames with Span::origin() (synthetic stdlib/builtin frames)
    // are NOT shown in error display output
    let def_span = test_span(3, 5, 3, 10);
    let mat_span = test_span(20, 1, 20, 5);
    let real_frame_span = test_span(10, 2, 10, 8);

    let mut err = EvalError::key_not_found(
        "missing_key".to_string(), vec![], def_span,
    ).with_materialization_span(mat_span);

    err.push_frame("user_function".to_string(), real_frame_span);
    err.push_frame("stdlib_internal".to_string(), Span::origin());  // <-- filtered
    err.push_frame("another_user_function".to_string(), real_frame2_span);

    let display = format!("{err}");
    
    // Real frames visible
    assert!(display.contains("in user_function at 10:2-10:8"));
    assert!(display.contains("in another_user_function at 15:1-15:12"));

    // Origin frame NOT visible
    assert!(!display.contains("stdlib_internal"));
    assert!(!display.contains("1:1-1:1"));
}
```

### Verdict
Span::origin() frame filtering is fully implemented and tested. Synthetic stdlib/builtin frames with 0:0-0:0 spans are correctly filtered from user-facing error output. **No work needed.**

---

## Summary

| Task | Status | Action Taken |
|------|--------|--------------|
| 1. Wire TypeAnnotationTable in builtin_load/expand | ✅ FIXED | Added typecheck call after resolve in both builtins |
| 2. Audit boundary_guards propagation | ✅ CLEAN | No issues found; all paths preserve guards correctly |
| 3. Wire TypeAnnotationTable lookup in force_step | ✅ ALREADY IMPLEMENTED | Lowering stage checks table; force_step uses resolved type |
| 8. Verify Span::origin() frame filtering | ✅ ALREADY IMPLEMENTED | Filter implemented in should_display_frame; tested |

## TODO.md Updates

Marked 4 items as complete in TODO.md:
- Line 740: builtin_load/expand TypeAnnotationTable wiring
- Line 741: boundary_guards propagation audit
- Line 742: force_step TypeAnnotationTable lookup
- Line 753: Span::origin() frame filtering verification

## Cross-Layer Integration Verification

### Pipeline Consistency
All entry points now follow the same pipeline order:
```
parse → expand → desugar → resolve → typecheck → eval
```

Typechecking populates the TypeAnnotationTable. Lowering consults the table to decide between `CoreExpr::TypeAssert` (with resolved type) and `CoreExpr::RuntimeTypeCheck` (nominal fallback). Force_step uses the resolved type when present.

### Builtins Creating Value::Program
Only 2 builtins create Value::Program:
1. `builtin_load` — now typechecks (fixed)
2. `builtin_expand` — now typechecks (fixed)

`builtin_eval` receives Value::Program as input and extracts its tables; it does not create new programs.

### Error Quality
Span::origin() filter ensures that synthetic builtin call frames (0:0-0:0) do not pollute user-facing stack traces. Real stdlib frames (with actual source spans from prelude.llt) are still shown, which is correct for debugging macro transformers and stdlib code.

---

**Files Modified:**
- src/builtins_meta.rs (lines 1721-1724, 1783-1785)
- TODO.md (lines 740-742, 753)

**Files Audited (no changes needed):**
- src/main.rs (boundary_guards propagation)
- src/repl.rs (boundary_guards propagation)
- src/lsp/document.rs (boundary_guards propagation)
- src/lower.rs (TypeAnnotationTable lookup)
- src/eval_materialize.rs (force_step TypeAssert/RuntimeTypeCheck handling)
- src/error.rs (Span::origin() frame filtering)
