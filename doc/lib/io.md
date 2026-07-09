# io

### `read-lines`

Read a file as a lazy sequence of lines

```tinct
fn@Dict [let cap@DirCap path@String]
```

### `println`

Print a string followed by a newline to stdout

```tinct
fn@Null [let s@String]
```

### `println-val`

Print any value as a string followed by a newline to stdout

```tinct
fn@Null [let v@Any]
```

### `write-file`

Write a string to a file (creates or truncates)

```tinct
fn@Null [let cap@DirCap path@String content@String]
```

### `write-file-atomic`

Atomically write a string to a file (write-to-temp + rename)

```tinct
fn@Null [let cap@DirCap path@String content@String]
```

### `write-line`

Write a line to a WriteHandle, appending a newline and flushing

```tinct
fn@Null [let handle@WriteHandle line@String]
```

### `append-file`

Append content to a file (creates if doesn't exist)

```tinct
fn@Null [let cap@DirCap path@String content@String]
```

### `open-write`

Open a file for writing (creates or truncates, returns handle)

```tinct
fn@WriteHandle [let cap@DirCap path@String]
```

### `open-append`

Open a file for appending (creates if doesn't exist, returns handle)

```tinct
fn@WriteHandle [let cap@DirCap path@String]
```

### `write-lines`

Write a sequence of lines to a WriteHandle

```tinct
fn@WriteHandle [let handle@WriteHandle lines@Seq]
```

### `has-cap?`

Check if a capability is present on a Handle or WriteHandle

```tinct
fn@Boolean [let h@[Handle  WriteHandle] cap@String]
```

### `copy`

Copy a file from src to dst

```tinct
fn@Null [let cap@DirCap src@String dst@String]
```
