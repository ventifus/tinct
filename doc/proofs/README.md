# Mechanized Proofs

This directory contains proof obligation sketches for the Tinct language implementation.
Each sketch documents a key semantic property and its proof strategy; mechanized proofs
provide machine-checked guarantees that complement the test suite.

## Proof Assistants

Two proof assistants cover the relevant proof obligations:

- **Coq** — for operational semantics proofs (the Tinct evaluator maps naturally
  to a small-step reduction relation). Coq's extraction to OCaml can produce
  a reference interpreter.
- **Isabelle/HOL** — for equational reasoning about the type system (row
  polymorphism unification correctness, principal type theorems).

## How to Contribute

1. Pick a proof obligation from the list below (or a `.md` file in this directory).
2. Write the formalization in Coq or Isabelle.
3. Place the proof file next to the sketch (`thunk_lifecycle.v` alongside `thunk_lifecycle.md`).
4. Open a PR with the `.md` updated to reflect proof status (`Status: proved`).

## Proof Obligations

| File | Property | Tool | Status |
|------|----------|------|--------|
| `thunk_lifecycle.md` | Thunk settlement monotonicity and evaluate-at-most-once sharing | Coq | Sketch |

Additional proof sketches will be added as formal properties are identified.

## Relationship to Tests

Mechanized proofs and tests are complementary:

- Tests provide fast regression coverage for concrete examples.
- Proofs establish universal properties that no finite test suite can cover.

When a proof is completed, the proof file notes the proof assistant version used.
