# regex

### `re-compile`

Compile a pattern (currently a no-op, just returns the pattern)

```tinct
fn@Dict [let pattern@String]
```

### `re-match`

Test if string contains pattern anywhere

```tinct
fn@Bool [let pattern@Unknown haystack@String]
```

### `re-find`

Find first match

```tinct
fn@Dict [let pattern@Unknown haystack@String]
```

### `re-findall`

Find all matches (returns dict for simplicity)

```tinct
fn@Dict [let pattern@Unknown haystack@String]
```

### `re-replace`

Replace all matches

```tinct
fn@String [let pattern@Unknown replacement@String haystack@String]
```

### `re-split`

Split on pattern

```tinct
fn@Dict [let pattern@Unknown haystack@String]
```

### `re-escape-replacement`

Escape replacement string

```tinct
fn@String [let s@String]
```

