# protocols/grpc

### `build-grpc-frame`

Build a gRPC Length-Prefixed Message frame

```tinct
fn@String [let data@String compressed@Bool]
```

### `parse-grpc-frame-header`

Parse gRPC Length-Prefixed Message header

```tinct
fn@Dict [let bytes@String]
```
