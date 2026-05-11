# Worked Examples

### 13.1 Simple Dict

**Input:**
```tinct
[name: "Alice"  age: 30]
```

**AST:**
```json
Dict([
    Entry { key: Some(Str("name")), value: Str("Alice") },
    Entry { key: Some(Str("age")),  value: Int(30) },
])
```

Note: Entries with explicit `key:` syntax produce `Entry { key: Some(...), value: ... }` nodes. The dict preserves insertion order for iteration.

### 13.2 Simple List

**Input:**
```tinct
["a" "b" "c"]
```

**AST:**
```json
Dict([
    Entry { key: None, value: Str("a") },
    Entry { key: None, value: Str("b") },
    Entry { key: None, value: Str("c") },
])
```

Note: Unkeyed entries produce `Entry { key: None, value: ... }` nodes. The evaluator assigns auto-incrementing integer keys `0, 1, 2, ...` during evaluation.

### 13.3 Nested Dict

**Input:**
```tinct
[
    database: [host: "localhost"  port: 5432]
    api: [endpoint: "/v1"]
]
```

**AST:**
```json
Dict([
    Entry {
        key: Some(Str("database")),
        value: Dict([
            Entry { key: Some(Str("host")), value: Str("localhost") },
            Entry { key: Some(Str("port")), value: Int(5432) },
        ])
    },
    Entry {
        key: Some(Str("api")),
        value: Dict([
            Entry { key: Some(Str("endpoint")), value: Str("/v1") },
        ])
    },
])
```

Note: Nested dicts are simply `Dict` expressions appearing as entry values. Letrec scoping allows entries to reference each other at any nesting level.

### 13.4 Function Call with Named Args

**Input:**
```tinct
[fetch "https://example.com" timeout: 60 retries: 3]
```

**AST:**
```json
Call {
    func: VarRef("fetch"),
    args: [Str("https://example.com")],
    named_args: [
        NamedArg { name: "timeout", value: Int(60) },
        NamedArg { name: "retries", value: Int(3) },
    ],
}
```

Note: Named arguments appear in a separate `named_args` list in the Call AST node. The evaluator binds named args after positional args via the C-PRIORITY chain.

### 13.5 Function Definition with Annotations

**Input:**
```tinct
[fn@Number [x@Number  y@[type: Number  default: 0]] [+ x y]]
```

**AST:**
```json
Fn {
    return_ann: Some(Annotation::Simple("Number")),
    params: [
        Param { name: "x", annotation: Some(Simple("Number")), variadic: false },
        Param { name: "y", annotation: Some(PropertyDict([
            Entry { key: Some(Str("type")), value: Str("Number") },
            Entry { key: Some(Str("default")), value: Int(0) },
        ])), variadic: false },
    ],
    body: Call {
        func: VarRef("+"),
        args: [VarRef("x"), VarRef("y")],
        named_args: [],
    },
}
```

Note: Parameter annotations can be simple type names (`Simple("Number")`) or property dicts containing `type`, `default`, or `description` fields. The type checker validates annotations against the inferred parameter types.

### 13.6 Pipeline with `_` Shorthand

**Input:**
```tinct
[-> data.users
    [filter [> _.age 30] _]
    [map _.name _]
    sort]
```

Note: `_` desugaring is a **pre-typecheck AST transformation**, not a parser or evaluator concern. The parser produces the AST as-is — `_` is just `VarRef("_")`. A desugaring pass (`desugar_file()` and `desugar_expr()`) then rewrites `_`-containing expressions into implicit lambdas before type checking and evaluation. See doc/04-functions.md §`_` Desugaring for the formal rewrite rules and scope boundary definition.

**AST:**
```json
Call {
    func: VarRef("->"),
    args: [
        DotAccess { expr: VarRef("data"), field: "users" },
        Call {
            func: VarRef("filter"),
            args: [
                Call {
                    func: VarRef(">"),
                    args: [
                        DotAccess { expr: VarRef("_"), field: "age" },
                        Int(30),
                    ],
                    named_args: [],
                },
                VarRef("_"),
            ],
            named_args: [],
        },
        Call {
            func: VarRef("map"),
            args: [
                DotAccess { expr: VarRef("_"), field: "name" },
                VarRef("_"),
            ],
            named_args: [],
        },
        VarRef("sort"),
    ],
    named_args: [],
}
```

Note: After desugaring, `_`-containing expressions become `Fn { params: [Param { name: "_", ... }], body: ... }` nodes. The desugaring happens before type checking.

### 13.7 Access Chains

**Input:**
```tinct
[get 0 config.services].host
```

**AST:**
```json
DotAccess {
    expr: Call {
        func: VarRef("get"),
        args: [Int(0), DotAccess { expr: VarRef("config"), field: "services" }],
    },
    field: "host",
}
```

Note: Bracket access is not supported. The old `config.services[0].host` syntax is written as `[get 0 config.services].host`. Access chains parse as nested AST nodes; the evaluator reduces inside-out, materializing each target before the next projection.

### 13.8 Subsequence Operations (replaces Range Access)

**Input:**
```tinct
[slice data 2 5]
```

**AST:**
```json
Call {
    func: VarRef("slice"),
    args: [VarRef("data"), Int(2), Int(5)],
}
```

Note: Range access syntax (`data[2..5]`) is not supported. Use the `slice`, `take`, and `drop` stdlib functions for subsequences. `[slice data 2 5]` returns entries at positions 2, 3, 4 (half-open interval). `[take 3 data]` returns the first 3 entries. `[drop 2 data]` returns all entries after the first 2.

### 13.9 Type Assertion

**Input:**
```tinct
[@Number expr]
```

**AST:**
```json
TypeAssert {
    annotation: Annotation::Simple("Number"),
    expr: VarRef("expr"),
}
```

Note: `TypeAssert` nodes materialize the inner expression and check its type. Type assertions are strict — they materialize the value immediately.

### 13.10 Type Assertion with Fallback

**Input:**
```tinct
[@[type: Number  default: 0] config.port]
```

**AST:**
```json
TypeAssert {
    annotation: Annotation::PropertyDict([
        Entry { key: Some(Str("type")), value: Str("Number") },
        Entry { key: Some(Str("default")), value: Int(0) },
    ]),
    expr: DotAccess { expr: VarRef("config"), field: "port" },
}
```

Note: Property dict annotations allow fallback defaults. If type checking fails, the evaluator uses the `default` value instead of erroring.

### 13.11 Type Alias

**Input:**
```tinct
Mapper: [type [Fn@b [a]]]
```

**AST:**
```json
Entry {
    key: Some(Str("Mapper")),
    value: TypeAlias(
        Dict([
            Entry { key: None, value: Annotated { name: "Fn", annotation: Simple("b") } },
            Entry { key: None, value: Dict([Entry { key: None, value: Str("a") }]) },
        ])
    ),
}
```

The type checker interprets `Annotated { name: "Fn", ... }` as a function type constructor.

### 13.12 Comments

**Input:**
```tinct
[
    # Configuration
    host: "localhost"  # server hostname
    port: 8080         # server port
]
```

**AST:**
```json
Dict([
    Entry { key: Some(Str("host")), value: Str("localhost") },
    Entry { key: Some(Str("port")), value: Int(8080) },
])
```

Note: Comments are discarded during tokenization and do not appear in the AST.

### 13.13 Variadic Function

**Input:**
```tinct
[fn [f ...args] [map f args]]
```

**AST:**
```json
Fn {
    return_ann: None,
    params: [
        Param { name: "f", annotation: None, variadic: false },
        Param { name: "args", annotation: None, variadic: true },
    ],
    body: Call {
        func: VarRef("map"),
        args: [VarRef("f"), VarRef("args")],
        named_args: [],
    },
}
```

Note: Variadic parameters (marked with `...`) collect all remaining positional arguments into a dict. The `variadic: true` flag signals this to the evaluator.

### 13.14 Mixed Positional and Named Entries

**Input:**
```tinct
[f x y timeout: 60]
```

**AST:**
```json
Call {
    func: VarRef("f"),
    args: [VarRef("x"), VarRef("y")],
    named_args: [
        NamedArg { name: "timeout", value: Int(60) },
    ],
}
```

Note: Call syntax distinguishes positional (`args`) and named (`named_args`) arguments. The parser places keyed arguments in `named_args`, unkeyed arguments in `args`.

### 13.15 Multi-Expression Document

**Input:**
```tinct
[x: 10]

[y: [+ x 1]]
```

**AST:**
```json
File {
    documents: [
        Document {
            expressions: [
                Dict([Entry { key: Some(Str("x")), value: Int(10) }]),
                Dict([Entry { key: Some(Str("y")), value: Call { func: VarRef("+"), args: [VarRef("x"), Int(1)], named_args: [] } }]),
            ]
        }
    ]
}
```

Note: Multiple top-level expressions in a document form a scope chain: each expression's dict becomes the parent scope for the following expression. Names from earlier expressions are visible via parent lookup but can be shadowed. Only the last expression's value is returned. See [Documents](09-documents.md) §Within a Document: Scope Chains.

### 13.16 Multi-Document File

**Input:**
```tinct
[data: [name: "Alice"  age: 30]]
---
[result: %.data]
```

**AST:**
```json
File {
    documents: [
        Document {
            expressions: [
                Dict([Entry { key: Some(Str("data")), value: Dict([...]) }])
            ]
        },
        Document {
            expressions: [
                Dict([Entry { key: Some(Str("result")), value: DotAccess { expr: VarRef("%"), field: "data" } }])
            ]
        }
    ]
}
```

Note: The `---` separator creates separate documents. The `%` pipeline reference accesses the result of the previous document. Each document is evaluated independently.
