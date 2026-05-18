# Test Literate Weave

## Test 1: Success with output

```tinct
1 + 2
```

## Test 2: Type warning (non-strict mode allows this)

```tinct
{x: 1} + {y: 2}
```

## Test 3: Eval error

```tinct
1 / 0
```

## Test 4: Success without warnings

```tinct
[1, 2, 3]
```
