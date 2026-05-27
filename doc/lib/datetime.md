# datetime

### `days-between`

Calculate days between two timestamps

```tinct
fn@Int [let a@Timestamp b@Timestamp]
```

### `timestamp-in-range?`

Check if a timestamp is within a range

```tinct
fn@Bool [let t@Timestamp start@Timestamp end@Timestamp]
```

### `format-date`

Format a timestamp as YYYY-MM-DD HH:MM:SS

```tinct
fn@String [let t@Timestamp]
```

