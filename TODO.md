# Implementation Roadmap

See DONE.md for the full history of completed sprints.

For future work beyond the active sprints below, see:
- `doc/whatif/index.md §Adopt Now` — features ready to implement (String Interpolation Phase 1, Let Binding, Structural Contracts Phase 1, ADTs Phase 1, Source Snippets, Circular Dep Error Paths, Eval Semantics Verification Phase 1)
- `doc/whatif/index.md §Wait for Trigger` — features with complete designs pending a concrete trigger

## Language Design Research

- [x] Research null semantics — see doc/whatif/null-semantics.md
- [x] Research tinct-hosted formatter — see doc/whatif/tinct-hosted-formatter.md and doc/whatif/ast-schema.md
- [x] Research macro-rewrite — see doc/whatif/macro-rewrite.md
- [x] Research parse-stage macros — see doc/whatif/parse-stage-macros.md
- [ ] Research Boolean-Algebraic Subtyping (BAS) as alternative foundation for D2 algebraic subtyping — Chau & Parreaux (POPL 2026) proves BAS encodes extensible records without row variables (one new term form, one typing rule, no subtyping changes). Complete soundness proofs. May supersede Marques et al. (2024) which lacks proofs. Write whatif evaluating BAS vs Rémy row variables for tinct's record model. Post-typing-cluster. Paper: doi:10.1145/3776689, preprint: https://lptk.github.io/files/boolean-algebraic-subtyping.pdf
