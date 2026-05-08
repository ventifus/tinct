# What If: Shared Value Traversal for Output Serializers

**State:** Proposal

What would it take to eliminate the duplicated traversal logic across tinct's Rust-level value serializers?

## Current State

`src/lib.rs` contains two value serializers — `value_to_json()` (lines 112–160) and `value_to_display_string()` (lines 162–211) — that traverse the same `Value` enum with nearly identical structural logic:

```rust
// Repeated structure in both functions:
match val {
    Value::Dict(map) => {
        // recurse into entries with depth check
    }
    Value::Seq { head, tail } => {
        // recurse through spine with depth check
    }
    Value::Int(n)    => /* render int */,
    Value::Float(f)  => /* render float */,
    Value::String { source, start, end } => /* render string */,
    Value::Bool(b)   => /* render bool */,
    Value::Dict(m) if m.is_empty() => /* render null */,
    Value::Function { .. } | Value::Builtin { .. } => /* render opaque */,
    ...
}
```

Both functions implement the same depth-limit guard, the same dict and seq traversal order, and the same recursive descent. They diverge only at leaf rendering: JSON requires RFC 8259 string escaping; display format uses tinct's `<builtin>` and `<function>` notation for opaque values, and pretty-prints booleans as `true`/`false` rather than JSON's lowercase equivalents.

This duplication has already caused bugs: a fix to depth limiting in one serializer was not propagated to the other.

### What's Missing

1. A single traversal implementation that both serializers share — ~50 lines of structural logic that currently exists twice
2. A clear extension point so future Rust-level serializers (e.g., a direct YAML renderer for the `--output yaml` fast path) can reuse the traversal without copy-pasting
3. Guaranteed propagation of structural fixes (depth limiting, cycle detection, Overlay flattening) to all output formats simultaneously

## Why Shared Traversal Matters for tinct

**Bug propagation is prevented.** Structural logic — depth limiting, Overlay flattening, Seq spine traversal — is implemented once. A fix or behavior change in the traversal applies to every output format automatically.

**New output formats are cheap.** A direct Rust-level YAML serializer (faster than the tinct-level `stdlib/out/yaml.llt` path for large values) requires only a new leaf renderer, not a new traversal.

**The implementation is small.** The shared traversal is ~50 lines of Rust. The payoff is immediate and the risk is minimal.

## Design

The shared traversal uses **closure callbacks** — not a trait object. A `ValueVisitor<Output>` trait adds ~40 lines of boilerplate, requires boxing or monomorphization decisions at every call site, and forces artificial API unification between formats that intentionally differ. Closures give the same structural sharing with zero abstraction overhead and are directly inlineable by the compiler.

```rust
/// Traverse a materialized Value, calling format callbacks at each leaf.
/// Handles: depth limiting, Overlay flattening, Seq spine, Dict entries.
///
/// `render_string`: receives raw (unescaped) string content
/// `render_int`, `render_float`, `render_bool`, `render_null`: obvious
/// `render_opaque`: receives "function" or "builtin" as the kind string
/// `wrap_dict`: called with the rendered entries string to produce e.g. "{...}"
/// `wrap_seq`: called with the rendered elements string to produce e.g. "[...]"
fn traverse_value(
    val: &Value,
    ctx: &Rc<EvalContext>,
    depth: usize,
    render_string:  &impl Fn(&str) -> String,
    render_int:     &impl Fn(i64) -> String,
    render_float:   &impl Fn(f64) -> String,
    render_bool:    &impl Fn(bool) -> String,
    render_null:    &impl Fn() -> String,
    render_opaque:  &impl Fn(&str) -> String,    // kind: "function" | "builtin"
    wrap_dict:      &impl Fn(Vec<(String, String)>) -> String,  // (key, value) pairs
    wrap_seq:       &impl Fn(Vec<String>) -> String,
) -> Result<String, Box<EvalError>>
```

The two existing serializers become thin wrappers:

```rust
pub fn value_to_json(val: &Value, ctx: &Rc<EvalContext>, depth: usize)
    -> Result<String, Box<EvalError>>
{
    traverse_value(val, ctx, depth,
        /* render_string */  &|s| format!("\"{}\"", json_escape(s)),
        /* render_int */     &|n| n.to_string(),
        /* render_float */   &|f| format_float_json(f),
        /* render_bool */    &|b| b.to_string(),
        /* render_null */    &|| "null".to_string(),
        /* render_opaque */  &|_| "null".to_string(),  // JSON has no function repr
        /* wrap_dict */      &|pairs| format!("{{{}}}", render_pairs_json(pairs)),
        /* wrap_seq */       &|elems| format!("[{}]", elems.join(",")),
    )
}

pub fn value_to_display_string(val: &Value, ctx: &Rc<EvalContext>, depth: usize)
    -> Result<String, Box<EvalError>>
{
    traverse_value(val, ctx, depth,
        /* render_string */  &|s| format!("\"{}\"", s),   // no escaping
        /* render_int */     &|n| n.to_string(),
        /* render_float */   &|f| f.to_string(),
        /* render_bool */    &|b| b.to_string(),
        /* render_null */    &|| "[]".to_string(),
        /* render_opaque */  &|kind| format!("<{}>", kind),
        /* wrap_dict */      &|pairs| format!("[{}]", render_pairs_display(pairs)),
        /* wrap_seq */       &|elems| format!("({} ...)", elems.first().unwrap_or(&"".into())),
    )
}
```

`traverse_value` owns the structural logic exclusively: depth guard, Overlay flattening (call `flatten_overlay` before dispatching), `Value::Dict` iteration in IndexMap insertion order, `Value::Seq` spine traversal via `run()`, and `Value::Handle`/`Value::NetCap`/`Value::DirCap` opaque handling.

## What Would Change

### `src/lib.rs`

**Current:** Two independent traversal functions (~110 lines combined) sharing no code.

**Proposed:** `fn traverse_value(...)` (~55 lines) + two thin wrappers (`value_to_json`, `value_to_display_string`, ~20 lines each). Net: eliminates ~50 lines of duplication, depth/Overlay logic lives in exactly one place.

**Impact:** Minor — internal refactor, no public API change. All existing callers of `value_to_json` and `value_to_display_string` are unchanged.

### Future serializers

**Current:** A new Rust-level serializer (e.g., direct YAML) must copy the full traversal.

**Proposed:** Provide closures to `traverse_value`. New serializer is ~20 lines.

**Impact:** Minor additive change per new format.

## Prerequisites

None. This is a pure internal refactor with no dependencies on other features or external changes. The duplication is already present and the fix is self-contained to `src/lib.rs`.

## References

- Gamma, E., Helm, R., Johnson, R., Vlissides, J. (1994). *Design Patterns: Elements of Reusable Object-Oriented Software.* Addison-Wesley. §Visitor — standard pattern; not used here because closure-based dispatch achieves the same structural sharing without trait-object overhead for fewer than ~5 implementations.
- Nickel source: `core/src/term/mod.rs` — uses `Display` impl directly on `Term`, no visitor. Confirms that a shared traversal function (not a trait) is the right scope for a small number of formats.
