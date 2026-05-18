# path

### `path-parts`

Split a path into its components

```tinct
fn@Dict [p@String]
```

### `basename`

Get the last component of a path (filename)

```tinct
fn@String [p@String]
```

### `dirname`

Get the directory portion of a path (all but the last component)

```tinct
fn@String [p@String]
```

### `extension`

Get the file extension (after the last .)

```tinct
fn@String [p@String]
```

### `path-join`

Join path components with /

```tinct
fn@String [parts@Dict]
```

