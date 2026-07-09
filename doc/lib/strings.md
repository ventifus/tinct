# strings

### `str-at`

Character at position n (0-based; negative counts from end). Raises error if out of bounds.

```tinct
fn@String [let n@Integer s@String]
```

### `str-substr`

Extract substring starting at start with length len. Raises error if bounds are invalid.

```tinct
fn@String [let start@Integer len@Integer s@String]
```

### `upper`

Convert a string to uppercase

```tinct
fn@String [let s@String]
```

### `lower`

Convert a string to lowercase

```tinct
fn@String [let s@String]
```

### `pad-left`

Left-pad a string to a target width with a padding character

```tinct
fn@String [let s width pad-char]
```

### `pad-right`

Right-pad a string to a target width with a padding character

```tinct
fn@String [let s width pad-char]
```

### `str-reverse`

Reverse a string by characters

```tinct
fn@String [let s@String]
```

### `str-replace`

Replace all occurrences of pattern with replacement in input

```tinct
fn@String [let pattern@String replacement@String input@String]
```

### `str-join`

Join collection elements as strings with a separator

```tinct
fn@String [let sep@String xs]
```
