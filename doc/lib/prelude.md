# prelude

### `variant?`

Check if value is a Variant.

```tinct
fn@Bool [let v]
```

### `payload-of`

[unindent "\nExtract the payload dict from a Variant value.\n\nExample: [payload-of [Ok 42]] => [a: 42] (the payload dict)\n\nNote: Materializes the Variant's payload by treating the Variant as a Dict\n(auto-unpack semantics). For unit variants (no payload), returns an empty dict.\nUsed by json.llt to serialize Variant payloads.\n"]

```tinct
fn@Any [let v]
```

### `words`

Split string on spaces into words

```tinct
fn@Seq [let s@String]
```

### `unindent`

Strip common leading indentation from a multi-line string

```tinct
fn@String [let s@String]
```

### `empty?`

Check if collection is empty

```tinct
fn@Bool [let xs]
```

### `make-entry`

Construct single-entry dict from key and value

```tinct
fn@Dict [let k v]
```

### `set`

Set key in dict

```tinct
fn@Dict [let xs@Dict k v]
```

### `update`

Update dict value by applying function

```tinct
fn@Dict [let xs@Dict k f@Fn]
```

### `nth`

Get nth element (supports negative indices)

```tinct
fn@Any [let xs@Dict n@Int]
```

### `conj`

Append element to end of list

```tinct
fn@Dict [let xs@Dict x]
```

### `sort-by`

Sort with custom comparator

```tinct
fn@Dict [let cmp@Fn xs@Dict]
```

### `sorted`

Sort collection in ascending order

```tinct
fn@Dict [let xs]
```

### `sorted-by`

Sort with custom comparator (accepts Seq or Dict)

```tinct
fn@Dict [let cmp@Fn xs]
```

### `fold`

Left fold (alias for reduce)

```tinct
fn@a [let f@Fn init@a xs]
```

### `flatten-impl-dict`

Flatten dict implementation (internal helper)

```tinct
fn@Dict [let xs@Dict]
```

### `find-deep-impl`

Find-deep implementation (internal helper)

```tinct
fn@Any [let xs@Dict target ks i@Int]
```

### `find-deep-check`

Find-deep check (internal helper)

```tinct
fn@Any [let xs@Dict target ks i@Int current-key]
```

### `find-deep-try`

Find-deep try (internal helper)

```tinct
fn@Any [let subtree@Dict target parent@Dict ks i@Int]
```

### `find-deep-try-check`

Find-deep try-check (internal helper)

```tinct
fn@Any [let result parent@Dict target ks i@Int]
```

### `find-first`

Find first element matching predicate; errors if none found

```tinct
fn@a [let pred@Fn xs]
```

### `find-first-or`

Find first matching element or return default

```tinct
fn@a [let pred@Fn default@a xs]
```

### `deep-merge-step`

Deep-merge step (internal helper)

```tinct
fn@Dict [let a@Dict b@Dict e]
```

### `walk-dict`

Walk nested dict structure (internal helper)

```tinct
fn@Dict [let f@Fn xs@Dict]
```

### `count`

Count elements satisfying predicate

```tinct
fn@Int [let pred@Fn xs]
```

### `contains?`

Check if collection contains element

```tinct
fn@Bool [let xs val]
```

### `uniq`

Remove duplicates (keep first occurrence). Still O(n²) due to O(n) contains-seq? per element, but uses O(1) cons instead of O(n) append.

```tinct
fn@Dict [let xs@Dict]
```

### `foldr`

Right fold

```tinct
fn@a [let f@Fn acc@a xs]
```

### `int?`

Check if value is Int

```tinct
fn@Any [let x]
```

### `float?`

Check if value is Float

```tinct
fn@Any [let x]
```

### `str?`

Check if value is String

```tinct
fn@Any [let x]
```

### `bool?`

Check if value is Bool

```tinct
fn@Any [let x]
```

### `null?`

Check if value is null (empty dict [])

```tinct
fn@Any [let x]
```

### `dict?`

Check if value is Dict

```tinct
fn@Any [let x]
```

### `fn?`

Check if value is a function (Function or Builtin)

```tinct
fn@Any [let x]
```

### `proxy?`

Check if value is a Proxy

```tinct
fn@Any [let x]
```

### `seq?`

Check if value is a Seq

```tinct
fn@Any [let x]
```

### `bytes?`

Check if value is Bytes

```tinct
fn@Any [let x]
```

### `num?`

Check if value is numeric (Int or Float)

```tinct
fn@Bool [let x]
```

### `list?`

Check if dict has all integer keys

```tinct
fn@Bool [let xs]
```

### `maybe-map`

Map over Maybe value

```tinct
fn@Any [let f ma]
```

### `<`

Less than

```tinct
fn@Bool [let x@a y@a]
```

### `=`

Equality

```tinct
fn@Bool [let x@a y@a]
```

### `+`

Addition

```tinct
fn@Number [let a@Number b@Number]
```

### `-`

Subtraction

```tinct
fn@Number [let a@Number b@Number]
```

### `*`

Multiplication

```tinct
fn@Number [let a@Number b@Number]
```

### `/`

Division

```tinct
fn@Number [let a@Number b@Number]
```

### `if`

Conditional (select branch by condition)

```tinct
fn@Any [let c t e]
```

### `raise`

[unindent "\nRaise a user error with the given message string.\n\nExample: [raise \"something went wrong\"]\n\nNote: Always fails. The error message must be a String. Use [try f] to catch\nuser errors from zero-arg functions.\n"]

```tinct
fn@Any [let msg@String]
```

### `filter`

Keep elements matching predicate

```tinct
fn@Any [let pred@Fn xs]
```

### `map`

Apply function to each element

```tinct
fn@Any [let f@Fn xs]
```

### `reduce`

Reduce collection with binary function

```tinct
fn@Any [let f@Fn init xs]
```

### `take`

Take first n elements

```tinct
fn@Any [let n@Int xs]
```

### `drop`

Drop first n elements

```tinct
fn@Any [let n@Int xs]
```

### `length`

[unindent "\nNumber of entries in a dict, or character count of a string.\n\nExample: [length [a: 1  b: 2  c: 3]] => 3\nExample: [length \"hello\"] => 5\nExample: [length []] => 0\n\nNote: For Seq values, prefer [count identity xs] (length forces a Seq to a Dict first).\n"]

```tinct
fn@Any [let xs]
```

### `collect-kv`

Reconstruct dict from key-value pairs

```tinct
fn@Dict [let xs]
```

### `str-contains?`

Check if haystack contains needle

```tinct
fn@Bool [let haystack@Stringing needle@Stringing]
```

### `starts-with?`

Check if string starts with prefix

```tinct
fn@Bool [let s@Stringing prefix@Stringing]
```

### `ends-with?`

Check if string ends with suffix

```tinct
fn@Bool [let s@Stringing suffix@Stringing]
```

### `str-repeat`

Repeat string n times

```tinct
fn@Stringing [let s@Stringing n@Int]
```

### `str-find`

Find first occurrence of needle in haystack; returns byte index or -1

```tinct
fn@Int [let haystack@Stringing needle@Stringing]
```

### `to-json`

Serialize a tinct value to a compact JSON string.
Handles Int, Float, Bool, String, Null ([]), Dict, and Seq values.
Proxy values raise an error (cannot serialize to JSON).
Other unserializable values (Fn, Handle, Task, etc.) produce null.

```tinct
fn@Any [let v]
```

### `to-json-pretty`

Serialize a tinct value to a pretty-printed JSON string with 2-space indentation.
Handles Int, Float, Bool, String, Null ([]), Dict, and Seq values.
Proxy values raise an error (cannot serialize to JSON).
Other unserializable values (Fn, Handle, Task, etc.) produce null.
Empty dicts render as {}, empty arrays as [] (compact, no newlines).

```tinct
fn@Any [let v]
```

### `non-negative`

Check if value is non-negative

```tinct
fn@Bool [let v]
```

### `positive`

Check if value is positive

```tinct
fn@Bool [let v]
```

### `lines`

Lazily read lines from a Handle. Returns a Seq of String, one per line. Strips trailing newline.

```tinct
fn@Any [let h]
```

### `chunks`

Lazily read fixed-size byte chunks from a Handle. Returns a Seq of Bytes.

```tinct
fn@Any [let h n@Int]
```
