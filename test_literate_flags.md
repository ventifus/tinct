# Test Literate Flags

This is a test document for the new literate flags.

## Basic evaluation

```tinct
[x: 1 y: 2]
[+ x y]
```

## With === sections

```tinct
[+ 1 2]
=== out
3
```

## Error embedding test

```tinct
[this-will-fail: [/ 1 0]]
```
