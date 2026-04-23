# Patterns & Comparisons

## Common Patterns

### Shared Base Config

```tinct
[
    base: [timeout: 30  retries: 3]
    dev:  [call $merge $base [env: dev]]
    prod: [call $merge $base [env: prod  timeout: 60]]
]
```

### List Transformation

```tinct
[
    users: [...]

    # Filtering and projection
    admin-users: [call $filter [fn [u] $u.is-admin] $users]
    user-names:  [call $map [fn [u] $u.name] $users]

    # Complex predicates
    senior-admins: [call $filter [fn [u] [call $and $u.is-admin [call $> $u.age 40]]] $users]
    user-summaries: [call $map [fn [u] [n: $u.name  a: $u.age]] $users]

    # List operations (renumber to clean list)
    reversed: [call $reverse $user-names]
    sorted: [call $sort $user-names]
    without-first: [call $rest $users]

    # Filter + reindex for clean dense list
    active: [call $-> $users
        [call $filter $_.active $_]
        $reindex]
]
```

### Conditional Logic

```tinct
[
    mode: production
    config: [call $if [call $= $mode production]
        [timeout: 60  logging: error]
        [timeout: 10  logging: debug]]

    # Conditional — returns body or [] (empty dict)
    debug-info: [call $when [call $= $mode dev] [trace: on  verbose: true]]
]
```

### Template Function

```tinct
[
    make-service: [fn@[name: String  port: Number  health: String] [name@String  port@Number]
        [name: $name  port: $port  health: "/health"]]

    web: [call $make-service web 8080]
    api: [call $make-service api 3000]
]
```

### Pipeline (Using Stdlib Threading)

```tinct
[call $-> $raw-data
    [call $filter $active? $_]
    [call $map $extract-name $_]
    [call $sort-by $last-name $_]]
```

---

## Comparison with Other Languages

See the comparison table below for how LLT relates to JSONnet, Dhall, Nix, CUE, and jq.

| Need | Use |
|------|-----|
| Universal compatibility | JSON |
| DevOps convention | YAML |
| Large-scale Kubernetes | JSONnet |
| Type-safe configs | Dhall |
| Package management | Nix |
| Schema validation | CUE |
| Shell JSON transforms | jq |
| **Unified data + transformation** | **LLT** |

### Data Selection: jq / JSONPath / JMESPath / LLT

| Operation | jq | JSONPath | JMESPath | LLT |
|-----------|-----|---------|----------|-----|
| Field access | `.name` | `$.name` | `name` | `$data.name` |
| Nested access | `.a.b.c` | `$.a.b.c` | `a.b.c` | `$data.a.b.c` |
| Deep path (dynamic) | `getpath(p)` | N/A | N/A | `[call $get-in $data $path]` |
| Computed key | `.["k"]` | `$['k']` | N/A | `$data[$key]` |
| Key index | `.["k"]` | `$[0]`, `$[1]` | `[0]`, `[-1]` | `$data[0]`, `$data[-1]` (key-based) |
| Positional index | `.[0]`, `.[-1]` | N/A | N/A | `[call $nth $data 0]`, `[call $nth $data -1]` |
| Key-range slice | N/A | N/A | N/A | `$data[2..5]` (keys in range) |
| Positional slice | `.[2:5]` | `$[2:5]` | `[2:5]` | `[call $slice $data 2 5]` |
| First/last | `.[0]`, `.[-1]` | `$[0]` | `[0]`, `[-1]` | `$data[0]` (key 0), `[call $last $data]` |
| Flatten | `flatten` | N/A | `[]` | `[call $flatten $list]` |
| All values | `.[]` | `$.*` | `*` | `[call $values $data]` |
| Filter (simple) | `select(.age > 30)` | `[?(@.age>30)]` | `` [?age>`30`] `` | `[call $filter [fn [u] [call $> $u.age 30]] $data]` |
| Filter (complex) | `select(.a and .b)` | `$[?@.a && @.b]` | `[?a && b]` | `[call $filter [fn [u] [call $and $u.a $u.b]] $data]` |
| Projection | `.items[].name` | `$.items[*].name` | `items[*].name` | `[call $map [fn [x] $x.name] $items]` |
| Reshape | `{n: .name}` | N/A | `{n: name}` | `[call $map [fn [x] [n: $x.name  a: $x.age]]]` |
| Multi-select | `[.name, .age]` | N/A | `[name, age]` | `[$data.name  $data.age]` |
| Pipe/chain | `\|` | implicit | `\|` | `[call $-> ...]` |
| Optional access | `.foo?` | N/A | N/A | `[call $get-or $data foo default]` |
| Existence check | `has("key")` | N/A | N/A | `[call $has? $data key]` |
| Recursive descent | `..` | `$..name` | N/A | `[call $find-deep $data name]` |

---

## Open Questions

### Structural Contracts

- [ ] **Shape/contract system** — Predicate-based validation separate from the type system. Allows runtime constraints beyond what types express (e.g., "port must be 1-65535").
- [ ] **OpenAPI integration** — Load external schemas as contracts for validation.
- [ ] **Lazy vs eager validation** — Validate on materialization vs explicit `[call $validate! $schema $data]`?
