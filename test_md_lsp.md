# Test Markdown LSP Support

This document tests LSP support for tinct blocks in markdown.

## Simple Block

```tinct
[x: 42]
```

## Block with Type Error

```tinct
[@Number "not a number"]
```

## Block with Parse Error

```tinct
[unterminated
```

## Valid Block with Function

```tinct
[
  add: [fn [a b] [+ a b]]
  result: [call $add 1 2]
]
```

## Block with Hover Test

```tinct
[
  identity: [fn [x] x]
  value: [call $identity 123]
]
```
