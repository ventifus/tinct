# protocols/dns

### `encode-dns-name`

Encode domain name in DNS label format (RFC 1035 §3.1)

```tinct
fn@String [let domain@String]
```

### `build-dns-query`

Build a DNS query message (RFC 1035 §4)

```tinct
fn@String [let id@Int domain@String qtype@Int]
```
