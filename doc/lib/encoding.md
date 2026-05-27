# encoding

### `hex-encode`

Convert a string to its hexadecimal representation

```tinct
fn@String [let s@String]
```

### `hex-decode`

Decode a hexadecimal string back to its original form

```tinct
fn@String [let s@String]
```

### `base64-encode`

Convert a string to base64 encoding

```tinct
fn@String [let s@String]
```

### `base64-decode`

Decode a base64 string back to its original form

```tinct
fn@String [let s@String]
```

### `mask-apply`

Apply an XOR mask to a string (repeating-key cipher)

```tinct
fn@String [let data@String mask@String]
```

