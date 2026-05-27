# desugar

### `desugar-program`

Desugar a Program by applying $_ implicit lambda transformation.

This function walks the Expression tree via match dispatch and wraps expressions
containing `$_` in non-parameter positions with implicit lambda functions.

The transformation is a surface pass: it runs after parsing and macro expansion,
before resolution and type checking.

Parameters:
  p@Program — The program to desugar

Returns:
  Program — The desugared program with all $_ references wrapped in lambdas

```tinct
fn@Any [let p@Program]
```

