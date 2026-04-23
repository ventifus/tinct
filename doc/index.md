# Tinct Language Documentation

## Language

1. [Introduction](01-introduction.md) — Vision, core principles, design philosophy
2. [Syntax](02-syntax.md) — Lexical grammar, syntactic grammar, pest rules, tokenization
3. [Data Model](03-data-model.md) — Dicts, lists, keys, numeric types, data access
4. [Functions](04-functions.md) — `call`, `fn`, arguments, variadic, `$_` desugaring, call convention
5. [Type Annotations](05-type-annotations.md) — `@` annotations, type assertions, type expressions, literal types
6. [Type Inference](06-type-inference.md) — Bidirectional typing, unification, subtyping, let-generalization
7. [Type System Extensions](07-type-extensions.md) — TypeAssert validation, row-variable unification, roadmap
8. [Evaluation](08-evaluation.md) — Lazy semantics, thunks, materialization, letrec, sequences
9. [Documents & Pipelines](09-documents.md) — `---` separators, `$$` isolation, scope chains, `$include`
10. [Error Handling](10-errors.md) — Exception model, `$try`, ErrorKind, error semantics
11. [Standard Library](11-stdlib.md) — Builtins, 62-function reference, equality, merge

## Tooling

12. [Tooling](12-tooling.md) — Formatter, sandboxing & security

## By Example

13. [Worked Examples](13-examples.md) — 16 annotated examples with AST output
14. [Patterns & Comparisons](14-patterns.md) — Common patterns, comparison with jq/JSONPath/JMESPath

## Internals

15. [AST & Parser Internals](15-ast.md) — AST node types, static constraints, desugaring rules
16. [Architecture](16-architecture.md) — Components, EvalContext, Value enum, compiler notes

## Appendix

17. [References](17-references.md) — Formal references, academic papers, resources
