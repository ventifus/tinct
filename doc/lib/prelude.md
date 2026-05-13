# prelude

### `not`

Boolean negation

```tinct
fn@Bool [x]
```

### `and`

Short-circuit AND

```tinct
fn@Unknown [a b]
```

### `or`

Short-circuit OR

```tinct
fn@Unknown [a b]
```

### `>`

Greater than

```tinct
fn@Bool [a b]
```

### `<=`

Less than or equal

```tinct
fn@Bool [a b]
```

### `>=`

Greater than or equal

```tinct
fn@Bool [a b]
```

### `quot`

Integer quotient (truncate toward zero)

```tinct
fn@Int [a@Number b@Number]
```

### `mod`

Integer remainder (sign of dividend)

```tinct
fn@Number [a@Number b@Number]
```

### `ceil`

Ceiling (smallest integer >= x)

```tinct
fn@Int [x@Number]
```

### `trunc`

Truncate toward zero

```tinct
fn@Int [x@Number]
```

### `abs`

Absolute value

```tinct
fn@Number [x@Number]
```

### `when`

Evaluate body if predicate is true

```tinct
fn@Unknown [pred body]
```

### `unless`

Evaluate body if predicate is false

```tinct
fn@Unknown [pred body]
```

### `cond`

Multi-branch conditional

```tinct
fn@Unknown [pairs@Dict]
```

### `get`

Get value by key with error if missing

```tinct
fn@Unknown [k xs@Dict]
```

### `empty?`

Check if collection is empty

```tinct
fn@Bool [xs]
```

### `make-entry`

Construct single-entry dict from key and value

```tinct
fn@Dict [k v]
```

### `set`

Set key in dict

```tinct
fn@Dict [xs@Dict k v]
```

### `remove`

Remove key from dict

```tinct
fn@Dict [xs@Dict k]
```

### `values`

Get all dict values as list

```tinct
fn@Dict [xs@Dict]
```

### `entries`

Get all entries as key-value pairs

```tinct
fn@Dict [xs@Dict]
```

### `from-entries`

Reconstruct dict from key-value pairs

```tinct
fn@Dict [pairs]
```

### `conj`

Append element to end of list

```tinct
fn@Dict [xs@Dict x]
```

### `reindex`

Rebuild with dense 0..n integer keys

```tinct
fn@Dict [xs@Dict]
```

### `map-entries`

Apply function to each key-value entry

```tinct
fn@Dict [f@Fn xs@Dict]
```

### `fold`

Left fold (alias for reduce)

```tinct
fn@Unknown [f@Fn init xs]
```

### `slice`

Take slice by position (preserves keys)

```tinct
fn@Dict [xs@Dict start@Int end@Int]
```

### `with-entries`

Transform dict via entries pipeline

```tinct
fn@Dict [xs@Dict f@Fn]
```

### `find-first`

Find first element matching predicate

```tinct
fn@Unknown [pred@Fn xs]
```

### `sum`

Sum all elements

```tinct
fn@Number [xs]
```

### `product`

Product of all elements

```tinct
fn@Number [xs]
```

### `compose`

Compose two functions

```tinct
fn@Fn [f@Fn g@Fn]
```

### `result-ok`

Wrap a value in Ok

```tinct
fn@Unknown [v]
```

### `assert`

Assert condition with error message

```tinct
fn@Unknown [cond msg@String]
```

### `<`

Less than

```tinct
fn@Bool [a b]
```

### `=`

Equality

```tinct
fn@Bool [a b]
```

### `+`

Addition

```tinct
fn@Number [a@Number b@Number]
```

### `-`

Subtraction

```tinct
fn@Number [a@Number b@Number]
```

### `*`

Multiplication

```tinct
fn@Number [a@Number b@Number]
```

### `/`

Division

```tinct
fn@Number [a@Number b@Number]
```

### `collect-kv`

Reconstruct dict from key-value pairs

```tinct
fn@Dict [xs]
```

### `between`

Predicate factory for range check (inclusive)

```tinct
fn@Fn [lo hi]
```

### `non-negative`

Check if value is non-negative

```tinct
fn@Bool [v]
```

### `positive`

Check if value is positive

```tinct
fn@Bool [v]
```

