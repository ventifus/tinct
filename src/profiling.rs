//! Profiling infrastructure: span collection, timing, and conversion to Value format.
//!
//! Profiling is opt-in via `--profile <file.json>`. When enabled, every thunk materialization
//! creates a SpanRecord tracking source location, timing, parent attribution, and stall breakdown.
//!
//! The ProfilingCollector maintains a stack of open span IDs to support nested materialization
//! and records both creation-context (when a thunk was allocated) and materialization-context
//! (when a thunk was forced). This dual attribution is essential for understanding lazy evaluation.

use std::sync::Arc;
use std::time::Instant;

use indexmap::IndexMap;

use crate::eval::EvalContext;
use crate::value::{string_val, Key, Thunk, Value};

/// Format a string as a tinct string literal with proper escaping.
///
/// Escapes `"`, `\`, `\n`, `\t`, `\r` and produces a quoted string.
fn format_tinct_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A single span record: one thunk materialization with full timing and attribution data.
#[derive(Debug, Clone)]
pub struct SpanRecord {
    /// Unique span ID (monotonically increasing from 0).
    pub id: u64,
    /// ID of the span that materialized this thunk (forcing context).
    pub materialize_parent: Option<u64>,
    /// ID of the span that was active when this thunk was created (allocation context).
    pub create_parent: Option<u64>,
    /// Wall-clock microseconds when the thunk was created (for flow arrows in traces).
    pub create_time_us: u64,
    /// Source file path; empty string for Rust builtins.
    pub source_file: Option<String>,
    /// Byte offset into source file (line, col) packed as `line * 1_000_000 + col`.
    pub source_start: Option<(usize, usize)>,
    /// Byte offset into source file (line, col) packed as `line * 1_000_000 + col`.
    pub source_end: Option<(usize, usize)>,
    /// Leading characters of source at this span (for display in traces).
    pub source_text: Option<String>,
    /// Builtin name (e.g., "builtin-map") if this is a Rust builtin.
    pub builtin_name: Option<String>,
    /// Originating Rust builtin for cross-boundary calls (e.g., tinct function called by builtin-map).
    pub origin_builtin: Option<String>,
    /// Wall-clock microseconds at materialization start (relative to baseline).
    pub start_us: u64,
    /// Wall-clock microseconds at materialization end (relative to baseline).
    pub end_us: u64,
    /// Microseconds blocked in I/O or async wait (subtracted from wall time to get CPU time).
    pub stall_us: u64,
    /// Stall cause: "io", "net", "channel", "timer", or None for compute spans.
    pub stall_kind: Option<String>,
}

impl SpanRecord {
    /// Serialize this span record to a tinct SCN dict format (one-line).
    ///
    /// Produces a dict with kebab-case keys matching the schema in doc/12-tooling.md.
    /// Empty optional fields serialize as `[]` (tinct null/empty-dict).
    /// Used for LLT-stream profiling output (one dict per line).
    pub fn to_tinct_line(&self) -> String {
        // Helper to format Option<String> as tinct string or [] (tinct null/absent sentinel)
        fn opt_str(s: &Option<String>) -> String {
            match s {
                Some(v) => format_tinct_string(v),
                None => "[]".to_string(),
            }
        }

        // Helper to format Option<u64> as tinct int or []
        fn opt_u64(v: Option<u64>) -> String {
            match v {
                Some(n) => n.to_string(),
                None => "[]".to_string(),
            }
        }

        // Helper to format Option<(line,col)> as packed int or 0
        fn opt_linecol(v: Option<(usize, usize)>) -> String {
            match v {
                Some((line, col)) => ((line * 1000000 + col) as i64).to_string(),
                None => "0".to_string(),
            }
        }

        // Build the dict in tinct SCN format with kebab-case keys
        format!(
            "[id: {}  materialize-parent: {}  create-parent: {}  create-time-us: {}  source-file: {}  source-start: {}  source-end: {}  source-text: {}  builtin: {}  origin-builtin: {}  start-us: {}  end-us: {}  stall-us: {}  stall-kind: {}]",
            self.id,
            opt_u64(self.materialize_parent),
            opt_u64(self.create_parent),
            self.create_time_us,
            opt_str(&self.source_file),
            opt_linecol(self.source_start),
            opt_linecol(self.source_end),
            opt_str(&self.source_text),
            opt_str(&self.builtin_name),
            opt_str(&self.origin_builtin),
            self.start_us,
            self.end_us,
            self.stall_us,
            opt_str(&self.stall_kind),
        )
    }

    /// Convert this span record to a tinct Value dict.
    ///
    /// Produces a dict with kebab-case keys matching the schema in doc/12-tooling.md.
    /// Empty optional fields use Value::Dict(IndexMap::new()) — the tinct empty-dict sentinel.
    /// Used for converting span sequences to Seq.Cons/Seq.Nil variants for final output.
    pub fn to_value(&self, ctx: &Arc<EvalContext>) -> Value {
        /// Allocate a materialized thunk into the arena and return the ThunkId.
        fn alloc(val: Value, ctx: &Arc<EvalContext>) -> crate::value::ThunkId {
            use crate::ast::Span;
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(val, Span::origin())))
        }

        /// The tinct empty-value sentinel (empty dict = `[]`).
        fn empty() -> Value {
            Value::Dict(IndexMap::new())
        }

        let mut entries: IndexMap<Key, crate::value::ThunkId> = IndexMap::new();

        entries.insert(
            Key::String("id".into()),
            alloc(Value::Int(self.id as i64), ctx),
        );

        entries.insert(
            Key::String("materialize-parent".into()),
            alloc(
                self.materialize_parent
                    .map(|id| Value::Int(id as i64))
                    .unwrap_or_else(empty),
                ctx,
            ),
        );

        entries.insert(
            Key::String("create-parent".into()),
            alloc(
                self.create_parent
                    .map(|id| Value::Int(id as i64))
                    .unwrap_or_else(empty),
                ctx,
            ),
        );

        entries.insert(
            Key::String("create-time-us".into()),
            alloc(Value::Int(self.create_time_us as i64), ctx),
        );

        entries.insert(
            Key::String("source-file".into()),
            alloc(
                self.source_file
                    .clone()
                    .map(|f| string_val(&f))
                    .unwrap_or_else(empty),
                ctx,
            ),
        );

        entries.insert(
            Key::String("source-start".into()),
            alloc(
                self.source_start
                    .map(|(line, col)| Value::Int((line * 1000000 + col) as i64))
                    .unwrap_or(Value::Int(0)),
                ctx,
            ),
        );

        entries.insert(
            Key::String("source-end".into()),
            alloc(
                self.source_end
                    .map(|(line, col)| Value::Int((line * 1000000 + col) as i64))
                    .unwrap_or(Value::Int(0)),
                ctx,
            ),
        );

        entries.insert(
            Key::String("source-text".into()),
            alloc(
                self.source_text
                    .clone()
                    .map(|t| string_val(&t))
                    .unwrap_or_else(empty),
                ctx,
            ),
        );

        entries.insert(
            Key::String("builtin".into()),
            alloc(
                self.builtin_name
                    .clone()
                    .map(|b| string_val(&b))
                    .unwrap_or_else(empty),
                ctx,
            ),
        );

        entries.insert(
            Key::String("origin-builtin".into()),
            alloc(
                self.origin_builtin
                    .clone()
                    .map(|o| string_val(&o))
                    .unwrap_or_else(empty),
                ctx,
            ),
        );

        entries.insert(
            Key::String("start-us".into()),
            alloc(Value::Int(self.start_us as i64), ctx),
        );
        entries.insert(
            Key::String("end-us".into()),
            alloc(Value::Int(self.end_us as i64), ctx),
        );
        entries.insert(
            Key::String("stall-us".into()),
            alloc(Value::Int(self.stall_us as i64), ctx),
        );

        entries.insert(
            Key::String("stall-kind".into()),
            alloc(
                self.stall_kind
                    .clone()
                    .map(|k| string_val(&k))
                    .unwrap_or_else(empty),
                ctx,
            ),
        );

        Value::Dict(entries)
    }
}

/// Profiling collector: records span data during evaluation.
///
/// Maintains a stack of open span IDs to track nested materialization and provides
/// methods to open/close spans, record stalls, and convert the collected spans to
/// a tinct Value (Seq of dicts) for JSON serialization.
#[derive(Debug)]
pub struct ProfilingCollector {
    /// All completed spans (open spans remain here with end_us = 0 until closed).
    spans: Vec<SpanRecord>,
    /// Stack of currently open span IDs (for materialize_parent tracking).
    open_stack: Vec<u64>,
    /// Next span ID to allocate (monotonically increasing).
    next_id: u64,
    /// Baseline timestamp for relative microsecond measurements.
    baseline: Instant,
    /// Index into `spans` of the first span not yet returned by `drain_new`.
    drain_cursor: usize,
}

impl ProfilingCollector {
    /// Create a new profiling collector with an empty span list and baseline timestamp.
    pub fn new() -> Self {
        Self {
            spans: Vec::new(),
            open_stack: Vec::new(),
            next_id: 0,
            baseline: Instant::now(),
            drain_cursor: 0,
        }
    }

    /// Open a new span and return its ID.
    ///
    /// Records the current time as start_us, allocates a unique span ID, and pushes
    /// the ID onto the open stack. The materialize_parent is the top of the stack
    /// before pushing (if any).
    #[allow(clippy::too_many_arguments)]
    pub fn open_span(
        &mut self,
        source_file: Option<String>,
        source_start: Option<(usize, usize)>,
        source_end: Option<(usize, usize)>,
        source_text: Option<String>,
        builtin_name: Option<String>,
        origin_builtin: Option<String>,
        create_parent: Option<u64>,
        create_time_us: u64,
    ) -> u64 {
        let id = self.next_id;
        self.next_id += 1;

        let materialize_parent = self.open_stack.last().copied();
        let start_us = self.baseline.elapsed().as_micros() as u64;

        self.spans.push(SpanRecord {
            id,
            materialize_parent,
            create_parent,
            create_time_us,
            source_file,
            source_start,
            source_end,
            source_text,
            builtin_name,
            origin_builtin,
            start_us,
            end_us: 0, // set on close
            stall_us: 0,
            stall_kind: None,
        });

        self.open_stack.push(id);
        id
    }

    /// Close a span by recording its end time and popping it from the stack.
    ///
    /// Panics if the span ID is not at the top of the stack (indicates mismatched open/close).
    pub fn close_span(&mut self, id: u64) {
        let end_us = self.baseline.elapsed().as_micros() as u64;

        // Pop from stack and verify it matches the expected ID
        match self.open_stack.pop() {
            Some(top_id) if top_id == id => {
                // Find the span and update its end_us
                if let Some(span) = self.spans.iter_mut().find(|s| s.id == id) {
                    span.end_us = end_us;
                }
            }
            Some(other_id) => {
                panic!(
                    "ProfilingCollector::close_span: expected id={}, got id={}",
                    other_id, id
                );
            }
            None => {
                panic!(
                    "ProfilingCollector::close_span: stack empty when closing id={}",
                    id
                );
            }
        }
    }

    /// Return the ID of the currently open span (top of stack), or None if no span is open.
    pub fn current_span_id(&self) -> Option<u64> {
        self.open_stack.last().copied()
    }

    /// Return the baseline Instant for relative time measurements.
    pub fn baseline_instant(&self) -> Instant {
        self.baseline
    }

    /// Record a stall (I/O wait) in the currently open span.
    ///
    /// Adds stall_us to the span's stall_us field and sets stall_kind if not already set.
    /// If multiple stalls occur in one span with different kinds, the first kind wins
    /// (this is a simplification; real analysis scripts use the Perfetto trace for
    /// per-stall detail).
    pub fn record_stall(&mut self, stall_us: u64, stall_kind: &str) {
        if let Some(&span_id) = self.open_stack.last() {
            if let Some(span) = self.spans.iter_mut().find(|s| s.id == span_id) {
                span.stall_us += stall_us;
                if span.stall_kind.is_none() {
                    span.stall_kind = Some(stall_kind.to_string());
                }
            }
        }
    }

    /// Clone current spans without consuming the collector.
    ///
    /// Returns a point-in-time snapshot suitable for background flushing. Open spans
    /// (those with `end_us == 0`) are included as-is — they will appear as in-progress
    /// entries in the JSON output. The collector continues accumulating normally after
    /// this call.
    pub fn snapshot(&self) -> Vec<SpanRecord> {
        self.spans.clone()
    }

    /// Returns spans added since the last drain, advancing the cursor.
    ///
    /// Each call returns only the spans appended after the previous call to `drain_new`.
    /// This is used by the LLT-stream background flush thread to write new spans incrementally
    /// without re-serializing spans that have already been written.
    pub fn drain_new(&mut self) -> Vec<SpanRecord> {
        let new_spans = self.spans[self.drain_cursor..].to_vec();
        self.drain_cursor = self.spans.len();
        new_spans
    }

    /// Extract all spans as a Vec, leaving the collector empty.
    /// This allows extracting profiling data without consuming the collector.
    pub fn extract_spans(&mut self) -> Vec<SpanRecord> {
        std::mem::take(&mut self.spans)
    }

    /// Convert all collected spans to a tinct Seq.Cons/Seq.Nil linked list of dicts.
    ///
    /// Each span becomes a dict with kebab-case keys matching the schema in doc/12-tooling.md.
    /// Empty optional fields use Value::Dict(IndexMap::new()) — the tinct empty-dict sentinel.
    pub fn into_value(self, ctx: &Arc<EvalContext>) -> Value {
        Self::spans_to_value(self.spans, ctx)
    }

    /// Convert a vector of spans to a tinct Seq.Cons/Seq.Nil linked list of dicts.
    /// Public to allow main.rs to serialize extracted spans.
    pub fn spans_to_value(spans: Vec<SpanRecord>, ctx: &Arc<EvalContext>) -> Value {
        /// Allocate a materialized thunk into the arena and return the ThunkId.
        fn alloc(val: Value, ctx: &Arc<EvalContext>) -> crate::value::ThunkId {
            use crate::ast::Span;
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(val, Span::origin())))
        }

        // Build the Seq from right to left (tail-first linked list).
        // Start from the Seq.Nil terminal and prepend each span dict.
        let mut acc: Value = crate::value::make_seq_nil();

        for s in spans.into_iter().rev() {
            let dict = s.to_value(ctx);
            let head_id = alloc(dict, ctx);
            let tail_id = alloc(acc, ctx);
            acc = crate::value::make_seq_cons(head_id, tail_id, ctx);
        }

        acc
    }
}

impl Default for ProfilingCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> Arc<EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        let stdlib_env = crate::builtins::create_stdlib_env().expect("stdlib failed");
        crate::eval::EvalContext::new(base_dir, Arc::clone(&stdlib_env), stdlib_env, false)
    }

    #[test]
    fn test_span_record_roundtrip() {
        let mut collector = ProfilingCollector::new();

        let id = collector.open_span(
            Some("test.llt".to_string()),
            Some((10, 5)),
            Some((10, 20)),
            Some("[+ 1 2]".to_string()),
            None,
            None,
            None,
            0,
        );

        collector.close_span(id);

        // Convert to Value — one span produces a non-empty Seq.Cons (head=dict, tail=Seq.Nil).
        let ctx = test_ctx();
        let value = collector.into_value(&ctx);
        match value {
            Value::Variant {
                ref tag,
                payload: Some(payload_id),
            } if tag == "Seq.Cons" => {
                // Extract head from Seq.Cons payload
                let payload_thunk = ctx.get_thunk(payload_id);
                let payload_val = payload_thunk
                    .try_get_materialized()
                    .expect("payload should be materialized");
                let head_id = match payload_val {
                    Value::Dict(ref d) => *d
                        .get(&Key::String("head".into()))
                        .expect("Seq.Cons must have head"),
                    _ => panic!("Seq.Cons payload should be a Dict"),
                };
                // head is the span dict thunk
                let head_thunk = ctx.get_thunk(head_id);
                let head_val = head_thunk
                    .try_get_materialized()
                    .expect("span dict should be materialized");
                match head_val {
                    Value::Dict(entries) => {
                        // Verify kebab-case keys exist
                        assert!(entries.contains_key(&Key::String("id".into())));
                        assert!(entries.contains_key(&Key::String("materialize-parent".into())));
                        assert!(entries.contains_key(&Key::String("create-parent".into())));
                        assert!(entries.contains_key(&Key::String("source-file".into())));
                        assert!(entries.contains_key(&Key::String("start-us".into())));
                        assert!(entries.contains_key(&Key::String("end-us".into())));
                    }
                    _ => panic!("Expected dict as head of Seq"),
                }
            }
            _ => panic!("Expected Seq.Cons for non-empty span list"),
        }
    }

    #[test]
    fn test_open_close_span() {
        let mut collector = ProfilingCollector::new();

        // Open parent span
        let parent_id = collector.open_span(
            Some("parent.llt".to_string()),
            Some((1, 1)),
            Some((1, 10)),
            None,
            None,
            None,
            None,
            0,
        );

        // Open child span — should have materialize_parent = parent_id
        let child_id = collector.open_span(
            Some("child.llt".to_string()),
            Some((2, 1)),
            Some((2, 10)),
            None,
            None,
            None,
            Some(parent_id),
            100,
        );

        // Close child first
        collector.close_span(child_id);

        // Verify child has correct materialize_parent
        let child_span = collector.spans.iter().find(|s| s.id == child_id).unwrap();
        assert_eq!(child_span.materialize_parent, Some(parent_id));
        assert_eq!(child_span.create_parent, Some(parent_id));

        // Close parent
        collector.close_span(parent_id);

        // Verify parent has no materialize_parent
        let parent_span = collector.spans.iter().find(|s| s.id == parent_id).unwrap();
        assert_eq!(parent_span.materialize_parent, None);
    }

    #[test]
    fn test_stall_recording() {
        let mut collector = ProfilingCollector::new();

        let id = collector.open_span(
            Some("io.llt".to_string()),
            Some((5, 1)),
            Some((5, 20)),
            None,
            None,
            None,
            None,
            0,
        );

        // Record two stalls
        collector.record_stall(1000, "io");
        collector.record_stall(500, "net");

        collector.close_span(id);

        // Verify stall_us accumulated
        let span = collector.spans.iter().find(|s| s.id == id).unwrap();
        assert_eq!(span.stall_us, 1500);
        // First stall kind wins
        assert_eq!(span.stall_kind.as_deref(), Some("io"));
    }

    #[test]
    fn test_to_tinct_line() {
        let span = SpanRecord {
            id: 42,
            materialize_parent: Some(10),
            create_parent: Some(5),
            create_time_us: 1000,
            source_file: Some("test.llt".to_string()),
            source_start: Some((10, 5)),
            source_end: Some((10, 20)),
            source_text: Some("[+ 1 2]".to_string()),
            builtin_name: None,
            origin_builtin: None,
            start_us: 2000,
            end_us: 3000,
            stall_us: 100,
            stall_kind: Some("io".to_string()),
        };

        let line = span.to_tinct_line();

        // Verify the line is a valid tinct dict with kebab-case keys
        assert!(line.starts_with("[id: 42"));
        assert!(line.contains("materialize-parent: 10"));
        assert!(line.contains("create-parent: 5"));
        assert!(line.contains("source-file: \"test.llt\""));
        assert!(line.contains("source-text: \"[+ 1 2]\""));
        assert!(line.contains("stall-kind: \"io\""));
        assert!(line.ends_with("]"));
    }

    #[test]
    fn test_to_tinct_line_with_empty_fields() {
        let span = SpanRecord {
            id: 1,
            materialize_parent: None,
            create_parent: None,
            create_time_us: 0,
            source_file: None,
            source_start: None,
            source_end: None,
            source_text: None,
            builtin_name: Some("builtin-map".to_string()),
            origin_builtin: None,
            start_us: 0,
            end_us: 100,
            stall_us: 0,
            stall_kind: None,
        };

        let line = span.to_tinct_line();

        // Verify empty optional fields serialize correctly
        // All absent optionals use [] (tinct null/absent sentinel), including string-typed fields.
        assert!(line.contains("materialize-parent: []"));
        assert!(line.contains("create-parent: []"));
        assert!(line.contains("source-file: []"));
        assert!(line.contains("source-text: []"));
        assert!(line.contains("builtin: \"builtin-map\""));
        assert!(line.contains("origin-builtin: []"));
        assert!(line.contains("stall-kind: []"));
    }
}
