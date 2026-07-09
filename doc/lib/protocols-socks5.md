# protocols/socks5

### `build-socks5-greeting`

Build SOCKS5 initial client greeting (RFC 1928 §3)

```tinct
fn@String [let methods@Dict]
```

### `build-socks5-connect`

Build SOCKS5 CONNECT request for host:port (RFC 1928 §4)

```tinct
fn@String [let host@String port@Integer]
```

### `parse-socks5-response`

Parse SOCKS5 server response (RFC 1928 §6)

```tinct
fn@Dict [let bytes@String]
```
