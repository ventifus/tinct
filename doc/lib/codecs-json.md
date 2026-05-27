# codecs/json

### `to-json`

Serialize any tinct value to a compact JSON string.
Expression, Document, and Program values use the Variant encoding {"Tag": {fields}}.
Primitive types serialize directly. Not every tinct type is serializable to JSON —
Fn, Handle, Task, Channel etc. produce null.

```tinct
fn@Any [let v]
```

### `to-json-pretty`

Serialize any tinct value to a pretty-printed JSON string with 2-space indentation.
Expression, Document, and Program values use the Variant encoding {"Tag": {fields}}.
Primitive types serialize directly. Not every tinct type is serializable to JSON —
Fn, Handle, Task, Channel etc. produce null.
Empty dicts render as {}, empty arrays as [] (compact, no newlines).

```tinct
fn@Any [let v]
```

### `from-json`

Parse a JSON string into a tinct value.

Parameters:
  s@String — JSON string to parse

Returns:
  Any — Parsed value (Dict, Seq, String, Int, Float, Bool, or [] for null)

Raises error on invalid JSON syntax.

```tinct
fn@Any [let s@String]
```

