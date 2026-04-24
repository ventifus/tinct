# Worked Examples

### 8.1 Simple Dict

**Input:**
```tinct
[name: Alice  age: 30]
```

**AST:**
```json
Dict([
    Entry { key: Some(Str("name")), value: Str("Alice") },
    Entry { key: Some(Str("age")),  value: Int(30) },
])
```

### 8.2 Simple List

**Input:**
```tinct
[a b c]
```

**AST:**
```json
Dict([
    Entry { key: None, value: Str("a") },
    Entry { key: None, value: Str("b") },
    Entry { key: None, value: Str("c") },
])
```

### 8.3 Nested Dict

**Input:**
```tinct
[
    database: [host: localhost  port: 5432]
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

### 8.4 Function Call with Named Args

**Input:**
```tinct
[call $fetch "https://example.com" timeout: 60 retries: 3]
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

### 8.5 Function Definition with Annotations

**Input:**
```tinct
[fn@Number [x@Number  y@[type: Number  default: 0]] [call $+ $x $y]]
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

### 8.6 Pipeline with `$_` Shorthand

**Input:**
```tinct
[call $-> $data.users
    [call $filter [call $> $_.age 30] $_]
    [call $map $_.name $_]
    $sort]
```

Note: `$_` desugaring is a **pre-typecheck AST transformation**, not a parser or evaluator concern. The parser produces the AST as-is — `$_` is just `VarRef("_")`. A desugaring pass (`desugar_file()` and `desugar_expr()`) then rewrites `$_`-containing expressions into implicit lambdas before type checking and evaluation. See doc/04-functions.md §`$_` Desugaring for the formal rewrite rules and scope boundary definition.

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

### 8.7 Access Chains

**Input:**
```tinct
$config.services[0].host
```

**AST:**
```json
DotAccess {
    expr: BracketAccess {
        expr: DotAccess {
            expr: VarRef("config"),
            field: "services",
        },
        key: Int(0),
    },
    field: "host",
}
```

### 8.8 Range Access

**Input:**
```tinct
$data[2..5]
```

**AST:**
```json
RangeAccess {
    expr: VarRef("data"),
    start: Some(Int(2)),
    end: Some(Int(5)),
}
```

### 8.9 Type Assertion

**Input:**
```tinct
[@Number $expr]
```

**AST:**
```json
TypeAssert {
    annotation: Annotation::Simple("Number"),
    expr: VarRef("expr"),
}
```

### 8.10 Type Assertion with Fallback

**Input:**
```tinct
[@[type: Number  default: 0] $config.port]
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

### 8.11 Type Alias

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

### 8.12 Comments

**Input:**
```tinct
[
    # Configuration
    host: localhost  # server hostname
    port: 8080       # server port
]
```

**AST:**
```json
Dict([
    Entry { key: Some(Str("host")), value: Str("localhost") },
    Entry { key: Some(Str("port")), value: Int(8080) },
])
```

Comments are discarded during tokenization and do not appear in the AST.

### 8.13 Variadic Function

**Input:**
```tinct
[fn [f ...args] [call $map $f $args]]
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

### 8.14 Mixed Positional and Named Entries

**Input:**
```tinct
[call $f $x $y timeout: 60]
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

### 8.15 Multi-Expression Document

**Input:**
```tinct
[x: 10]

[y: [call $+ $x 1]]
```

**AST:**
```json
File {
    documents: [
        Document {
            expressions: [
                Dict([Entry { key: Some(Str("x")), value: Int(10) }]),
                Dict([Entry { key: Some(Str("y")), value: Call { func: VarRef("+"), args: [VarRef("x"), Int(1)] } }]),
            ]
        }
    ]
}
```

### 8.16 Multi-Document File

**Input:**
```tinct
[data: [name: Alice  age: 30]]
---
[result: $$.data]
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
                Dict([Entry { key: Some(Str("result")), value: DotAccess { expr: VarRef("$"), field: "data" } }])
            ]
        }
    ]
}
```
