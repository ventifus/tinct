# io

### `read-file`

Read an entire file as a string

```tinct
(value)
```

### `read-lines`

Read a file as a lazy sequence of lines

```tinct
(value)
```

### `println`

Print a string followed by a newline to stdout

```tinct
fn@Any [s@String]
```

### `println-val`

Print any value as a string followed by a newline to stdout

```tinct
fn@Any [v@Any]
```

### `write-file`

Write a string to a file (creates or truncates)

```tinct
fn@Any [cap@DirCap path@String content@String]
```

### `write-file-atomic`

Atomically write a string to a file (write-to-temp + rename)

```tinct
fn@Any [cap@DirCap path@String content@String]
```

### `write-line`

Write a line to a WriteHandle, appending a newline and flushing

```tinct
(value)
```

### `append-file`

Append content to a file (creates if doesn't exist)

```tinct
fn@Any [cap@DirCap path@String content@String]
```

### `open-write`

Open a file for writing (creates or truncates, returns handle)

```tinct
fn@WriteHandle [cap@DirCap path@String]
```

### `open-append`

Open a file for appending (creates if doesn't exist, returns handle)

```tinct
fn@WriteHandle [cap@DirCap path@String]
```

### `write-lines`

Write a sequence of lines to a WriteHandle

```tinct
fn@WriteHandle [handle@WriteHandle lines@Seq]
```

### `has-cap?`

Check if a capability is present on a Handle or WriteHandle

```tinct
fn@Bool [h cap]
```

### `copy`

Copy a file from src to dst

```tinct
(value)
```

