# protocols/websocket

### `build-ws-frame`

Build a masked WebSocket frame (client→server, RFC 6455)

```tinct
fn@Dict [let opcode@Int data@String mask-key@String]
```

### `parse-ws-frame-header`

Parse WebSocket frame header bytes (RFC 6455)

```tinct
fn@Dict [let bytes@String]
```

### `build-ws-handshake`

Build HTTP/1.1 WebSocket upgrade request (RFC 6455 §4.1)

```tinct
fn@String [let host@String path@String key@String]
```

