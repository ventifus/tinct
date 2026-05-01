# Mechanized Proofs

This directory contains proof sketches and planned mechanized verification artifacts for
the Tinct language implementation. Mechanized proofs provide machine-checked guarantees
that complement the test suite.

## Status

Stub — no mechanized proofs yet. The files here are proof obligation sketches that
document the intended properties and their proof strategies.

## Planned Proof System

Two proof assistants are under consideration:

- **Coq** — preferred for operational semantics proofs (the Tinct evaluator maps naturally
  to a small-step reduction relation). Coq's extraction to OCaml could eventually produce
  a reference interpreter.
- **Isabelle/HOL** — preferred for equational reasoning about the type system (row
  polymorphism unification correctness, principal type theorems).

## How to Contribute

1. Pick a proof obligation from the list below (or a `.md` file in this directory).
2. Write the formalization in Coq or Isabelle.
3. Place the proof file next to the sketch (`thunk_lifecycle.v` alongside `thunk_lifecycle.md`).
4. Open a PR with the `.md` updated to reflect proof status (`Status: proved`).

## Proof Obligations (planned)

| File | Property | Tool | Status |
|------|----------|------|--------|
| `thunk_lifecycle.md` | Thunk bisimulation — lazy and eager evaluation produce the same result when the value is forced | Coq | Sketch |
| `type_soundness.md` | Progress + preservation for the Hindley-Milner core (no row polymorphism yet) | Coq | Planned |
| `row_unification.md` | Row unification terminates and produces the principal unified type | Isabelle | Planned |
| `desugar_sound.md` | `$_` desugaring preserves semantics — desugared AST evaluates to the same value | Coq | Planned |

## Relationship to Tests

Mechanized proofs and tests are complementary:

- Tests provide fast regression coverage for concrete examples.
- Proofs establish universal properties that no finite test suite can cover.

When a proof is completed, the corresponding TODO.md item should be marked `[x]`
and the proof file should note the proof assistant version used.
