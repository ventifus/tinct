//! Runtime value types: `Value`, `Thunk` (lazy memoization), `Environment` (legacy name chain), `Scope` (runtime scope via `ScopeArena`).

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use indexmap::{Equivalent, IndexMap};

use crate::ast::{CoreExpr, Param, Span, Spanned, SurfaceDocument, SurfaceNode};
use crate::error::{EvalError, EvalResult};
use crate::types::Type;

// Re-export ThunkId for use in other modules
pub use crate::arena::ThunkId;

/// Type alias for the optional default expression + environment pair in guarded thunks.
/// Reduces type_complexity in UnevaluatedState::Guarded and Thunk constructors.
/// `env_id` is the u32 index into ScopeArena for the environment in which the default is evaluated.
type GuardDefault = (Arc<Spanned<CoreExpr>>, u32);

/// Runtime metadata for user-defined functions — stored on `Value::Function`.
/// Enables runtime reflection via `ast-of` builtin and LSP features (hover, go-to-def).
#[derive(Clone, Debug)]
pub struct FnAnnotation {
    /// Doc string extracted from function's annotation metadata dict.
    pub doc: Option<String>,
    /// Source file path where the function was defined (if available).
    pub source_file: Option<String>,
    /// Return type annotation from the function's `@[...]` declaration.
    /// None for unannotated functions.
    pub return_ann: Option<crate::ast::Annotation>,
    /// Source span of the function definition (for AST-of and LSP go-to-definition).
    pub source_span: crate::ast::Span,
    /// Extra annotation fields from `@[key: val ...]` that are not standard (doc, return, etc.).
    /// Evaluated at definition time. Used by TypeNode protocol (as-type:, etc.).
    pub extra: indexmap::IndexMap<String, crate::value::Value>,
}

/// Arguments passed to built-in functions.
///
/// Owns its arguments to allow capture in `async move` blocks that must be `'static`.
/// Previously used `&[Arc<Thunk>]` (a borrow), which caused lifetime errors when moved
/// into `Box<dyn Future>` (which has an implicit `'static` bound). Using owned `Vec`
/// avoids allocating lifetimes in the async state machine.
pub struct BuiltinArgs {
    pub args: Vec<ThunkId>,
    pub named: Option<IndexMap<String, ThunkId>>,
    pub call_span: Span,
    pub ctx: Arc<crate::eval::EvalContext>,
    /// Caller's scope id — present only when `BuiltinDef::needs_caller_env` is true.
    /// None for the vast majority of builtins that do not need lexical scope access.
    /// Panics in `builtin-current-env` if unexpectedly None (indicates a registration bug).
    pub caller_env_id: Option<u32>,
}

/// Signature for built-in functions: receives a `BuiltinArgs` struct containing
/// positional args, named args, and call-site span.
/// Returns a Future that resolves to an `Arc<Thunk>` to allow builtins to participate
/// in lazy evaluation and async operations.
///
/// Note: `+ Send` is intentionally absent. LLT uses a `current_thread` runtime
/// (see `async_rt.rs`), so futures never cross thread boundaries. Builtins capture
/// `Rc<...>`-containing types (e.g. `Arc<Thunk>`, `Value`) that are `!Send`; requiring
/// `+ Send` would force unsafe workarounds with no actual thread-safety benefit.
pub type BuiltinFn = fn(BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>>;

/// Strictness annotation for builtin argument demand (Wadler & Hughes 1987).
#[repr(u8)]
#[non_exhaustive]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Strictness {
    /// W&H "id" — identity projection. Argument never forced at dispatch site.
    Id,
    /// W&H "seq" — force to WHNF before builtin is called.
    Seq,
    /// W&H spine projection — force structural layer without element values.
    Spine,
}

/// Builtin function definition with strictness metadata.
#[derive(Copy, Clone)]
pub struct BuiltinDef {
    pub func: BuiltinFn,
    pub name: &'static str,
    pub pos_strictness: &'static [Strictness],
    /// Number of positional args to pre-materialize unconditionally before dispatch.
    /// Independent of pos_strictness W1 scanning. Default 0 (no forced args).
    pub force_count: usize,
    /// Whether this builtin needs the caller's lexical scope id.
    /// When false (the default for almost all builtins), `BuiltinArgs.caller_env_id` is None.
    /// When true, `BuiltinArgs.caller_env_id` is Some(env_id).
    /// Only `builtin-current-env` (and similar scope-introspecting builtins) set this to true.
    pub needs_caller_env: bool,
}

impl PartialEq for BuiltinDef {
    fn eq(&self, other: &Self) -> bool {
        // Compare by name only — function pointer comparison is unreliable
        self.name == other.name
    }
}

impl Eq for BuiltinDef {}

impl fmt::Debug for BuiltinDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltinDef")
            .field("name", &self.name)
            .field("pos_strictness", &self.pos_strictness)
            .field("force_count", &self.force_count)
            .field("needs_caller_env", &self.needs_caller_env)
            .finish_non_exhaustive()
    }
}

/// Dict key type: either an integer (auto-indexed) or a string (bare word / quoted).
/// This is the canonical hashable key used in `Value::Dict` and `IndexMap<HashableValue, Arc<Thunk>>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashableValue {
    Int(i64),
    Str(Arc<str>),
}

impl PartialOrd for HashableValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (HashableValue::Int(a), HashableValue::Int(b)) => a.partial_cmp(b),
            (HashableValue::Str(a), HashableValue::Str(b)) => a.partial_cmp(b),
            _ => None, // mixed types are incomparable
        }
    }
}

impl Hash for HashableValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Use explicit u8 discriminants (Int=0u8, Str=2u8) instead of
        // std::mem::discriminant so that StrHashableValue::hash can use the same
        // literal without allocating a temporary Arc<str>.
        match self {
            HashableValue::Int(n) => {
                0u8.hash(state);
                n.hash(state);
            }
            HashableValue::Str(s) => {
                2u8.hash(state);
                s.hash(state);
            }
        }
    }
}

impl fmt::Display for HashableValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HashableValue::Int(n) => write!(f, "{n}"),
            HashableValue::Str(s) => write!(f, "{s}"),
        }
    }
}

/// A wrapper type for `&str` that hashes the same way as `HashableValue::Str`.
/// This enables zero-allocation lookups in `IndexMap<HashableValue, V>`.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct StrHashableValue<'a>(pub &'a str);

impl Hash for StrHashableValue<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // HashableValue::Str is discriminant 2u8 (Int=0, Str=2).
        // Using 2u8 directly avoids the Arc::from("") allocation that the
        // std::mem::discriminant approach required on every hash call.
        2u8.hash(state);
        // Then hash the string content
        self.0.hash(state);
    }
}

impl Equivalent<HashableValue> for StrHashableValue<'_> {
    fn equivalent(&self, key: &HashableValue) -> bool {
        match key {
            HashableValue::Str(s) => self.0 == s.as_ref(),
            HashableValue::Int(_) => false,
        }
    }
}

/// Network capability allowlist entry (Miller 2006 object capability model).
/// Matches hostnames, ports, and IP ranges at connect time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetCapEntry {
    /// Exact hostname match (case-insensitive), any port.
    /// Example: "api.example.com"
    Hostname(String),
    /// Exact hostname and port match.
    /// Example: "api.example.com:443"
    HostPort(String, u16),
    /// Hostname glob with prefix wildcard only.
    /// Example: "*.internal"
    HostnameGlob(String),
    /// CIDR range (IPv4 or IPv6).
    /// Example: "192.168.1.0/24", "2001:db8::/32"
    Cidr(ipnet::IpNet),
    /// Unrestricted — allow any host/port.
    /// Produced by `--cap-net NAME=any` on the CLI.
    Any,
}

/// Clock capability inner implementation (Miller 2006 object capability model).
/// Allows scripts to read the current time only if they receive a ClockCap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClockCapInner {
    /// Real system clock — reads std::time::SystemTime::now()
    Real,
    /// Fixed timestamp — always returns this nanosecond value (for testing)
    Fixed(i64),
}

/// Directory capability permissions (Miller 2006 principle of least authority).
/// Fine-grained permission flags for DirCap — purely additive (no flags means no access).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirPerms {
    pub readable: bool,
    pub statable: bool,
    pub listable: bool,
    pub writable: bool,
    pub appendable: bool,
    pub deletable: bool,
    pub renameable: bool,
    pub symlinkable: bool,
    pub posix_permissions: bool,
    pub extended_attributes: bool,
}

impl DirPerms {
    /// Full access — all permissions granted.
    pub fn full() -> Self {
        Self {
            readable: true,
            statable: true,
            listable: true,
            writable: true,
            appendable: true,
            deletable: true,
            renameable: true,
            symlinkable: true,
            posix_permissions: true,
            extended_attributes: true,
        }
    }

    /// Read-only access — read files, list directories, stat metadata.
    pub fn read_only() -> Self {
        Self {
            readable: true,
            statable: true,
            listable: true,
            writable: false,
            appendable: false,
            deletable: false,
            renameable: false,
            symlinkable: false,
            posix_permissions: false,
            extended_attributes: false,
        }
    }

    /// Parse a single letter mode (r/w/a/s/l/y) and return the corresponding permissions.
    pub fn from_letter(c: char) -> Option<Self> {
        match c {
            'r' => Some(Self {
                readable: true,
                statable: true,
                listable: true,
                writable: false,
                appendable: false,
                deletable: false,
                renameable: false,
                symlinkable: false,
                posix_permissions: false,
                extended_attributes: false,
            }),
            'w' => Some(Self {
                readable: false,
                statable: false,
                listable: false,
                writable: true,
                appendable: true,
                deletable: true,
                renameable: true,
                symlinkable: false,
                posix_permissions: false,
                extended_attributes: false,
            }),
            'a' => Some(Self {
                readable: false,
                statable: false,
                listable: false,
                writable: false,
                appendable: true,
                deletable: false,
                renameable: false,
                symlinkable: false,
                posix_permissions: false,
                extended_attributes: false,
            }),
            's' => Some(Self {
                readable: false,
                statable: true,
                listable: false,
                writable: false,
                appendable: false,
                deletable: false,
                renameable: false,
                symlinkable: false,
                posix_permissions: false,
                extended_attributes: false,
            }),
            'l' => Some(Self {
                readable: false,
                statable: true,
                listable: true,
                writable: false,
                appendable: false,
                deletable: false,
                renameable: false,
                symlinkable: false,
                posix_permissions: false,
                extended_attributes: false,
            }),
            'y' => Some(Self {
                readable: false,
                statable: false,
                listable: false,
                writable: false,
                appendable: false,
                deletable: false,
                renameable: false,
                symlinkable: true,
                posix_permissions: false,
                extended_attributes: false,
            }),
            _ => None,
        }
    }

    /// Merge two DirPerms by union (additive composition).
    pub fn union(&self, other: &Self) -> Self {
        Self {
            readable: self.readable || other.readable,
            statable: self.statable || other.statable,
            listable: self.listable || other.listable,
            writable: self.writable || other.writable,
            appendable: self.appendable || other.appendable,
            deletable: self.deletable || other.deletable,
            renameable: self.renameable || other.renameable,
            symlinkable: self.symlinkable || other.symlinkable,
            posix_permissions: self.posix_permissions || other.posix_permissions,
            extended_attributes: self.extended_attributes || other.extended_attributes,
        }
    }
}

/// Transient mutable builder for efficient dict construction.
/// Provides O(1) insert/delete with a one-shot invariant: once frozen (via builder-finish),
/// all subsequent mutations return errors. The Option enables the frozen pattern: finish takes
/// the inner map, leaving None behind.
///
/// `frozen` is an `AtomicBool` fast-path: set to `true` in `finish()` BEFORE taking the
/// mutex. All read/write methods check `frozen.load(Relaxed)` first and return immediately
/// without acquiring the lock. This eliminates mutex contention on post-freeze reads (which
/// are the hot path in builder-heavy prelude code like `collect-kv`).
pub struct Builder {
    frozen: AtomicBool,
    inner: Mutex<Option<IndexMap<HashableValue, Arc<Thunk>>>>,
}

impl Builder {
    /// Create a new empty builder.
    pub fn new() -> Self {
        Self {
            frozen: AtomicBool::new(false),
            inner: Mutex::new(Some(IndexMap::new())),
        }
    }

    /// Create a new empty builder with pre-allocated capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            frozen: AtomicBool::new(false),
            inner: Mutex::new(Some(IndexMap::with_capacity(capacity))),
        }
    }

    /// Check if the builder is frozen (fast-path: atomic load, no mutex).
    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::Relaxed)
    }

    /// Set a key-value pair. Returns error if frozen.
    pub fn set(&self, key: HashableValue, value: Arc<Thunk>) -> Result<(), String> {
        // Fast-path: check frozen flag without taking the mutex.
        if self.frozen.load(Ordering::Relaxed) {
            return Err("builder is frozen (already finished)".to_string());
        }
        let mut guard = self.inner.lock().unwrap();
        match guard.as_mut() {
            Some(map) => {
                map.insert(key, value);
                Ok(())
            }
            None => Err("builder is frozen (already finished)".to_string()),
        }
    }

    /// Delete a key. Returns error if frozen.
    pub fn delete(&self, key: &HashableValue) -> Result<(), String> {
        // Fast-path: check frozen flag without taking the mutex.
        if self.frozen.load(Ordering::Relaxed) {
            return Err("builder is frozen (already finished)".to_string());
        }
        let mut guard = self.inner.lock().unwrap();
        match guard.as_mut() {
            Some(map) => {
                map.shift_remove(key);
                Ok(())
            }
            None => Err("builder is frozen (already finished)".to_string()),
        }
    }

    /// Check if a key exists. Returns false (not error) if frozen.
    pub fn has(&self, key: &HashableValue) -> bool {
        // Fast-path: frozen builder has no entries.
        if self.frozen.load(Ordering::Relaxed) {
            return false;
        }
        let guard = self.inner.lock().unwrap();
        guard.as_ref().is_some_and(|map| map.contains_key(key))
    }

    /// Get a value by key. Returns None if key doesn't exist or builder is frozen.
    pub fn get(&self, key: &HashableValue) -> Option<Arc<Thunk>> {
        // Fast-path: frozen builder has no entries.
        if self.frozen.load(Ordering::Relaxed) {
            return None;
        }
        let guard = self.inner.lock().unwrap();
        guard.as_ref().and_then(|map| map.get(key).cloned())
    }

    /// Atomically get-or-insert: if `key` exists, return its Arc<Thunk>; otherwise
    /// insert `default_thunk` at `key` and return `default_thunk`.
    /// Returns error if the builder is frozen.
    ///
    /// This eliminates the `builder-has?` + `builder-get` + `builder-set` triple
    /// that `group-by` previously used, reducing locking overhead and avoiding the
    /// race window between the has? check and the set.
    pub fn get_or(
        &self,
        key: HashableValue,
        default_thunk: Arc<Thunk>,
    ) -> Result<Arc<Thunk>, String> {
        // Fast-path: frozen builder cannot be mutated.
        if self.frozen.load(Ordering::Relaxed) {
            return Err("builder is frozen (already finished)".to_string());
        }
        let mut guard = self.inner.lock().unwrap();
        match guard.as_mut() {
            Some(map) => {
                if let Some(existing) = map.get(&key) {
                    Ok(Arc::clone(existing))
                } else {
                    map.insert(key, Arc::clone(&default_thunk));
                    Ok(default_thunk)
                }
            }
            None => Err("builder is frozen (already finished)".to_string()),
        }
    }

    /// Take the inner map, freezing the builder. Returns error if already frozen.
    pub fn finish(&self) -> Result<IndexMap<HashableValue, Arc<Thunk>>, String> {
        // Set the frozen flag BEFORE taking the mutex so that concurrent readers
        // on the fast-path see frozen=true as soon as possible.
        if self.frozen.swap(true, Ordering::Relaxed) {
            // Already frozen — swap returned the old value (true).
            return Err("builder is already frozen".to_string());
        }
        let mut guard = self.inner.lock().unwrap();
        guard
            .take()
            .ok_or_else(|| "builder is already frozen".to_string())
    }

    /// Clone the inner map without freezing. Returns error if frozen.
    pub fn snapshot(&self) -> Result<IndexMap<HashableValue, Arc<Thunk>>, String> {
        // Fast-path: no need to take the lock to know it's empty.
        if self.frozen.load(Ordering::Relaxed) {
            return Err("builder is frozen".to_string());
        }
        let guard = self.inner.lock().unwrap();
        guard
            .as_ref()
            .ok_or_else(|| "builder is frozen".to_string())
            .cloned()
    }
}

impl Clone for Builder {
    fn clone(&self) -> Self {
        let guard = self.inner.lock().unwrap();
        Self {
            frozen: AtomicBool::new(self.frozen.load(Ordering::Relaxed)),
            inner: Mutex::new(guard.clone()),
        }
    }
}

/// A materialized runtime value.
pub enum Value {
    /// 64-bit signed integer
    Int(i64),
    /// Unsigned 64-bit integer (from `42u`, `0xFFu` literals)
    U64(u64),
    /// 64-bit IEEE 754 float
    Float(f64),
    /// UTF-8 string (from bare words or quoted literals).
    /// Stored as a shared slice of a source string with byte offsets.
    /// This enables zero-copy substring operations and shared storage.
    String {
        source: Arc<str>,
        start: usize,
        end: usize,
    },
    /// Internal boolean value — used by Rust-level boolean predicates returning true/false
    Bool(bool),
    /// Ordered key-value map with lazy (thunked) values
    Dict(IndexMap<HashableValue, Arc<Thunk>>),
    /// Transient builder for efficient mutable dict construction.
    /// One-shot invariant: once frozen (via builder-finish), all mutations error.
    /// Sequential-use: not safe for concurrent modification (Mutex protects state, not semantics).
    Builder(Arc<Builder>),
    /// User-defined function (closure capturing its defining environment).
    /// `body` is stored as `Arc<Spanned<CoreExpr>>` (Parts-E migration: no Expr round-trip).
    /// `closure_env_id` is the ScopeId index into EvalContext.scope_arena for the closure scope.
    Function {
        params: Arc<Vec<Param>>,
        body: Arc<Spanned<CoreExpr>>,
        closure_env_id: u32,
        annotation: Option<Box<FnAnnotation>>,
    },
    /// Rust-native built-in function
    Builtin(BuiltinDef),
    /// Lazy linked-list sequence (head element, tail sequence)
    Seq { head: ThunkId, tail: ThunkId },
    /// Proxy object — field access calls the handler function with the field name
    Proxy { handler: ThunkId },
    /// Lazy overlay: R overrides L (right-biased merge). Flattened to Dict on demand.
    /// Construction is O(1) — neither L nor R is materialized at merge time.
    Overlay(ThunkId, ThunkId),
    /// Capability-bound directory handle (object capability model)
    DirCap {
        dir: cap_std::fs::Dir,
        perms: DirPerms,
    },
    /// Network capability — authority to connect to specified hosts/subnets
    NetCap(Arc<Vec<NetCapEntry>>),
    /// Raw OS file handle (thin wrapper over cap_std::fs::File, no buffering).
    /// Opened via `builtin-file-open`; read/written/sought via `builtin-file-*` builtins.
    File(Arc<Mutex<cap_std::fs::File>>),
    /// Revocable directory capability
    RevocableDirCap {
        inner: cap_std::fs::Dir,
        perms: DirPerms,
        revoked: Arc<AtomicBool>,
    },
    /// Nominal variant (enum-like value)
    Variant {
        tycon: Arc<str>,
        ctor: Arc<str>,
        payload: Option<ThunkId>,
    },
    /// Exact base-10 decimal (rust_decimal::Decimal, 96-bit software decimal).
    Decimal(rust_decimal::Decimal),
    /// Arbitrary-precision integer (num_bigint::BigInt).
    BigInt(num_bigint::BigInt),
    /// Byte sequence (opaque binary data).
    Bytes {
        source: Arc<[u8]>,
        start: usize,
        end: usize,
    },
    /// URI — a uniform resource identifier with scheme and URI string.
    Uri { scheme: String, uri: String },
    /// UTC timestamp (nanoseconds since Unix epoch).
    Timestamp(i64),
    /// Signed duration (nanoseconds).
    Duration(i64),
    /// Clock capability for reading current time (object capability model).
    ClockCap(Arc<ClockCapInner>),
    /// Timezone (parsed IANA TZ rules from zoneinfo file).
    Timezone(Arc<jiff::tz::TimeZone>),
    /// QUIC session — multiplexed connection over UDP (RFC 9000).
    QuicSession(Arc<quinn::Connection>),
    /// HTTP/2 session — multiplexed HTTP connection (RFC 9113).
    Http2Session {
        client: Arc<reqwest::Client>,
        base_url: String,
    },
    /// HTTP/3 session — HTTP over QUIC (RFC 9114).
    Http3Session(Arc<Mutex<Http3SessionState>>),
    /// QUIC datagram handle — unreliable message delivery over QUIC (RFC 9221).
    QuicDatagramHandle(Arc<quinn::Connection>),

    // =========================================================================
    // runtime-v2 native AST value types
    // =========================================================================
    /// A complete tinct program — the type returned by `builtin-parse` and related builtins.
    ///
    /// The `SurfaceProgram` AST is stored directly in an `Arc` for shared ownership.
    /// `resolutions`, `types`, and `expects_resolved` are populated by the resolve/typecheck
    /// pipeline stages and carried alongside the program for use by downstream builtins.
    Program {
        program: std::sync::Arc<crate::ast::SurfaceProgram>,
        resolutions: Arc<crate::ast::ResolutionTable>,
        types: Arc<crate::ast::TypeAnnotationTable>,
        expects_resolved: Arc<HashMap<crate::ast::Span, crate::types::Type>>,
    },

    /// A single document within a program — accessible via `program.documents`.
    Document(Arc<SurfaceDocument>),

    /// A single AST expression node — the type returned by `ast-of` and `[quote ...]`.
    Expression(Arc<SurfaceNode>),

    // =========================================================================
    // runtime-v2 async primitives
    // =========================================================================
    /// Async task handle — returned by `task` builtin, consumed by `await`.
    Task(Arc<tokio::sync::Mutex<TaskState>>),

    /// Channel for inter-task communication — created by `channel` builtin.
    Channel(Arc<ChannelInner>),

    /// Broadcast channel — created by `broadcast-channel` builtin.
    BroadcastChannel(Arc<BroadcastChannelInner>),

    /// Oneshot sender half — created by `oneshot-channel` builtin.
    OneshotSender(Arc<OneshotSenderInner>),

    /// Oneshot receiver half — created by `oneshot-channel` builtin.
    OneshotReceiver(Arc<OneshotReceiverInner>),

    /// Cancellation context — created by `context` builtin.
    Context(tokio_util::sync::CancellationToken),

    /// Reactive cell — created by `reactive-cell` builtin.
    ReactiveCell(Arc<ReactiveCellInner>),

    /// Arena view handle — wraps a named scope managed by this arena.
    /// `start_env_id` is the root scope allocated by `arena-new`.
    /// The actual end of the arena is always computed dynamically from `envs.len()` at
    /// drop/migrate time; there is no stored end field (it would be stale immediately).
    Arena { name: Arc<str>, start_env_id: u32 },
    /// Value annotated with runtime metadata (e.g. constructor annotation dict).
    /// Used by `make-annotated` and annotated unit constructors.
    /// `annotation` is a materialized `Value::Dict` of annotation key-value pairs.
    Annotated {
        inner: Box<Value>,
        annotation: Box<Value>,
    },
    /// Type-checker context handle — wraps `TypeContextData` for passing between tinct builtins.
    /// Created by `builtin-get-type-context`; consumed by `builtin-typecheck-doc`, `builtin-resolve`, etc.
    TypeContext(std::sync::Arc<std::sync::Mutex<crate::eval::TypeContextData>>),
    /// A fully-lowered document — produced by `builtin-lower` after lowering all SurfaceNodes to
    /// CoreExpr. This is the discrete lowering step that separates surface-to-core lowering from
    /// evaluation. Entries are (key_name, lowered_CoreExpr) pairs in document order.
    ///
    /// Treated as non-equal to everything (including itself) in PartialEq — CoreDocument is a
    /// pipeline-internal intermediate value; structural equality is meaningless at this stage.
    CoreDocument {
        entries: std::sync::Arc<
            Vec<(
                String,
                std::sync::Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
            )>,
        >,
        span: crate::ast::Span,
    },
}

impl Clone for Value {
    fn clone(&self) -> Self {
        match self {
            Value::Int(n) => Value::Int(*n),
            Value::U64(n) => Value::U64(*n),
            Value::Float(n) => Value::Float(*n),
            Value::String { source, start, end } => Value::String {
                source: Arc::clone(source),
                start: *start,
                end: *end,
            },
            Value::Bool(b) => Value::Bool(*b),
            Value::Dict(map) => Value::Dict(map.clone()),
            Value::Builder(b) => Value::Builder(Arc::clone(b)),
            Value::Function {
                params,
                body,
                closure_env_id,
                annotation,
            } => Value::Function {
                params: Arc::clone(params),
                body: Arc::clone(body),
                closure_env_id: *closure_env_id,
                annotation: annotation.clone(),
            },
            Value::Builtin(def) => Value::Builtin(*def),
            Value::Seq { head, tail } => Value::Seq {
                head: *head,
                tail: *tail,
            },
            Value::Proxy { handler } => Value::Proxy { handler: *handler },
            Value::Overlay(l, r) => Value::Overlay(*l, *r),
            Value::DirCap { dir, perms } => Value::DirCap {
                // SAFETY: DirCap values are always created from valid OS file descriptors
                // (main.rs and builtins_io.rs construction sites). try_clone() can fail with
                // EMFILE (too many open files) or EBADF (invalid descriptor) only if the
                // descriptor is closed or the fd table is exhausted. DirCap values are assumed
                // to remain valid for their Arc lifetime — the descriptor is never explicitly
                // closed while a DirCap holding it is alive.
                dir: dir.try_clone().expect("DirCap try_clone"),
                perms: perms.clone(),
            },
            Value::NetCap(entries) => Value::NetCap(Arc::clone(entries)),
            Value::File(f) => Value::File(Arc::clone(f)),
            Value::RevocableDirCap {
                inner,
                perms,
                revoked,
            } => Value::RevocableDirCap {
                inner: inner.try_clone().expect("RevocableDirCap try_clone"),
                perms: perms.clone(),
                revoked: Arc::clone(revoked),
            },
            Value::Variant {
                tycon,
                ctor,
                payload,
            } => Value::Variant {
                tycon: Arc::clone(tycon),
                ctor: Arc::clone(ctor),
                payload: *payload,
            },
            Value::Decimal(d) => Value::Decimal(*d),
            Value::BigInt(n) => Value::BigInt(n.clone()),
            Value::Bytes { source, start, end } => Value::Bytes {
                source: Arc::clone(source),
                start: *start,
                end: *end,
            },
            Value::Uri { scheme, uri } => Value::Uri {
                scheme: scheme.clone(),
                uri: uri.clone(),
            },
            Value::Timestamp(n) => Value::Timestamp(*n),
            Value::Duration(n) => Value::Duration(*n),
            Value::ClockCap(c) => Value::ClockCap(Arc::clone(c)),
            Value::Timezone(tz) => Value::Timezone(Arc::clone(tz)),
            Value::QuicSession(s) => Value::QuicSession(Arc::clone(s)),
            Value::Http2Session { client, base_url } => Value::Http2Session {
                client: Arc::clone(client),
                base_url: base_url.clone(),
            },
            Value::Http3Session(s) => Value::Http3Session(Arc::clone(s)),
            Value::QuicDatagramHandle(h) => Value::QuicDatagramHandle(Arc::clone(h)),
            Value::Program {
                program,
                resolutions,
                types,
                expects_resolved,
            } => Value::Program {
                program: std::sync::Arc::clone(program),
                resolutions: Arc::clone(resolutions),
                types: Arc::clone(types),
                expects_resolved: Arc::clone(expects_resolved),
            },
            Value::Document(d) => Value::Document(Arc::clone(d)),
            Value::Expression(e) => Value::Expression(Arc::clone(e)),
            Value::Task(t) => Value::Task(Arc::clone(t)),
            Value::Channel(c) => Value::Channel(Arc::clone(c)),
            Value::BroadcastChannel(c) => Value::BroadcastChannel(Arc::clone(c)),
            Value::OneshotSender(s) => Value::OneshotSender(Arc::clone(s)),
            Value::OneshotReceiver(r) => Value::OneshotReceiver(Arc::clone(r)),
            Value::Context(c) => Value::Context(c.clone()),
            Value::ReactiveCell(r) => Value::ReactiveCell(Arc::clone(r)),
            Value::Arena { name, start_env_id } => Value::Arena {
                name: Arc::clone(name),
                start_env_id: *start_env_id,
            },
            Value::Annotated { inner, annotation } => Value::Annotated {
                inner: inner.clone(),
                annotation: annotation.clone(),
            },
            Value::TypeContext(tc) => Value::TypeContext(Arc::clone(tc)),
            Value::CoreDocument { entries, span } => Value::CoreDocument {
                entries: std::sync::Arc::clone(entries),
                span: span.clone(),
            },
        }
    }
}

/// State of an async task spawned via `task` builtin.
pub enum TaskState {
    Pending(tokio::task::JoinHandle<EvalResult<Value>>),
    Done(EvalResult<Value>),
}

/// Inner state for a channel created via `channel` builtin.
pub struct ChannelInner {
    pub sender: tokio::sync::mpsc::Sender<Value>,
    pub receiver: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Value>>,
    pub capacity: i64,
}

/// Inner state for a broadcast channel created via `broadcast-channel` builtin.
pub struct BroadcastChannelInner {
    pub sender: tokio::sync::broadcast::Sender<Value>,
    pub capacity: i64,
}

/// Inner state for the sender half of a oneshot channel created via `oneshot-channel` builtin.
pub struct OneshotSenderInner {
    pub sender: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<Value>>>,
}

/// Inner state for the receiver half of a oneshot channel created via `oneshot-channel` builtin.
pub struct OneshotReceiverInner {
    pub receiver: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<Value>>>,
}

/// Inner state for a reactive cell created via `reactive-cell` builtin.
pub struct ReactiveCellInner {
    pub sender: tokio::sync::Mutex<tokio::sync::watch::Sender<Value>>,
    pub receiver: tokio::sync::watch::Receiver<Value>,
}

/// State for an HTTP/3 session.
pub struct Http3SessionState {
    pub send_request: h3::client::SendRequest<h3_quinn::OpenStreams, bytes::Bytes>,
    pub _driver: tokio::task::JoinHandle<()>,
}

/// Helper function to construct a `Value::String` from a string slice.
pub fn string_val(s: &str) -> Value {
    Value::String {
        source: Arc::from(s),
        start: 0,
        end: s.len(),
    }
}

/// Helper function to construct a `Value::Bytes` from a byte slice.
pub fn bytes_val(data: &[u8]) -> Value {
    Value::Bytes {
        source: Arc::from(data),
        start: 0,
        end: data.len(),
    }
}

impl Value {
    /// Returns a human-readable type name for error messages and diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "Int",
            Value::U64(_) => "Int",
            Value::Float(_) => "Float",
            Value::String { .. } => "String",
            Value::Bool(_) => "Bool",
            Value::Dict(_) => "Dict",
            Value::Builder(_) => "Builder",
            Value::Function { .. } => "Function",
            Value::Builtin(_) => "Builtin",
            Value::Seq { .. } => "Seq",
            Value::Proxy { .. } => "Proxy",
            Value::Overlay(..) => "Dict",
            Value::DirCap { .. } => "DirCap",
            Value::NetCap(_) => "NetCap",
            Value::File(_) => "File",
            Value::RevocableDirCap { .. } => "DirCap",
            Value::Variant { .. } => "Variant",
            Value::Decimal(_) => "Decimal",
            Value::BigInt(_) => "BigInt",
            Value::Bytes { .. } => "Bytes",
            Value::Uri { .. } => "Uri",
            Value::Timestamp(_) => "Timestamp",
            Value::Duration(_) => "Duration",
            Value::ClockCap(_) => "ClockCap",
            Value::Timezone(_) => "Timezone",
            Value::QuicSession(_) => "QuicSession",
            Value::Http2Session { .. } => "Http2Session",
            Value::Http3Session(_) => "Http3Session",
            Value::QuicDatagramHandle(_) => "QuicDatagramHandle",
            Value::Program { .. } => "Program",
            Value::Document(_) => "Document",
            Value::Expression(_) => "Expression",
            Value::Task(_) => "Task",
            Value::Channel(_) => "Channel",
            Value::BroadcastChannel(_) => "BroadcastChannel",
            Value::OneshotSender(_) => "OneshotSender",
            Value::OneshotReceiver(_) => "OneshotReceiver",
            Value::Context(_) => "Context",
            Value::ReactiveCell(_) => "ReactiveCell",
            Value::Arena { .. } => "Arena",
            Value::Annotated { inner, .. } => inner.type_name(),
            Value::TypeContext(_) => "TypeContext",
            Value::CoreDocument { .. } => "CoreDocument",
        }
    }

    /// Returns the tinct TyCon name for opaque builtin values.
    ///
    /// Opaque builtin types are Value variants that map to a declared tinct type but are
    /// NOT represented as Value::Variant at runtime. For these, type checking must use
    /// the declared TyCon name directly rather than checking the Variant tag prefix.
    ///
    /// Returns None for structural values (Dict, Seq, Int, String, Function, Variant, etc.)
    /// that are handled through the structural or TyConDef-constructor type-checking paths.
    pub fn value_tycon_name(&self) -> Option<&'static str> {
        match self {
            Value::Program { .. } => Some("Program"),
            Value::Document(_) => Some("Document"),
            Value::TypeContext(_) => Some("TypeContext"),
            // Both DirCap variants map to the declared "DirCap" type.
            Value::DirCap { .. } | Value::RevocableDirCap { .. } => Some("DirCap"),
            Value::NetCap(_) => Some("NetCap"),
            Value::File(_) => Some("File"),
            // type_name() returns "Builder" but the declared TyCon is "BuilderHandle".
            Value::Builder(_) => Some("BuilderHandle"),
            Value::Task(_) => Some("Task"),
            Value::Channel(_) => Some("Channel"),
            Value::Context(_) => Some("Context"),
            Value::ReactiveCell(_) => Some("ReactiveCell"),
            Value::ClockCap(_) => Some("ClockCap"),
            Value::Timezone(_) => Some("Timezone"),
            Value::Decimal(_) => Some("Decimal"),
            Value::BigInt(_) => Some("BigInt"),
            Value::QuicSession(_) => Some("QuicSession"),
            Value::Http2Session { .. } => Some("Http2Session"),
            Value::Http3Session(_) => Some("Http3Session"),
            // All other values (Int, String, Float, Bool, Dict, Overlay, Seq, Function,
            // Builtin, Variant, Bytes, Uri, Proxy, Annotated, etc.) are handled through
            // structural type checking or TyConDef constructor matching.
            _ => None,
        }
    }

    /// Extract a string slice from a `Value::String`, or `None` if not a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String { source, start, end } => Some(&source[*start..*end]),
            _ => None,
        }
    }

    /// Extract a byte slice from a `Value::Bytes`, or `None` if not bytes.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes { source, start, end } => Some(&source[*start..*end]),
            _ => None,
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(n) => f.debug_tuple("Int").field(n).finish(),
            Value::U64(n) => f.debug_tuple("U64").field(n).finish(),
            Value::Float(n) => f.debug_tuple("Float").field(n).finish(),
            Value::String { source, start, end } => {
                let s = &source[*start..*end];
                f.debug_tuple("String").field(&s).finish()
            }
            Value::Bool(b) => f.debug_tuple("Bool").field(b).finish(),
            Value::Dict(map) => {
                let keys: Vec<&HashableValue> = map.keys().collect();
                f.debug_tuple("Dict").field(&keys).finish()
            }
            Value::Builder(builder) => {
                if builder.is_frozen() {
                    write!(f, "Builder(frozen)")
                } else {
                    write!(f, "Builder")
                }
            }
            Value::Function { params, .. } => {
                let names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
                write!(f, "Function({})", names.join(", "))
            }
            Value::Builtin(def) => write!(f, "Builtin({})", def.name),
            Value::Seq { .. } => write!(f, "Seq(...)"),
            Value::Proxy { .. } => write!(f, "Proxy"),
            Value::Overlay(..) => write!(f, "Overlay(...)"),
            Value::DirCap { .. } => write!(f, "DirCap"),
            Value::NetCap(entries) => write!(f, "NetCap({} entries)", entries.len()),
            Value::File(_) => write!(f, "File"),
            Value::RevocableDirCap { revoked, .. } => {
                if revoked.load(Ordering::Acquire) {
                    write!(f, "DirCap(revoked)")
                } else {
                    write!(f, "DirCap(revocable)")
                }
            }
            Value::Variant {
                tycon,
                ctor,
                payload,
            } => {
                if payload.is_some() {
                    write!(f, "Variant({}.{}, <payload>)", tycon, ctor)
                } else {
                    write!(f, "Variant({}.{})", tycon, ctor)
                }
            }
            Value::Decimal(d) => write!(f, "Decimal({d})"),
            Value::BigInt(n) => write!(f, "BigInt({n})"),
            Value::Bytes { source, start, end } => {
                let bytes = &source[*start..*end];
                write!(f, "Bytes({} bytes)", bytes.len())
            }
            Value::Uri { scheme, uri } => write!(f, "Uri({scheme}:{uri})"),
            Value::Timestamp(nanos) => write!(f, "Timestamp({nanos} ns)"),
            Value::Duration(nanos) => write!(f, "Duration({nanos} ns)"),
            Value::ClockCap(inner) => match inner.as_ref() {
                ClockCapInner::Real => write!(f, "ClockCap(Real)"),
                ClockCapInner::Fixed(nanos) => write!(f, "ClockCap(Fixed({nanos} ns))"),
            },
            Value::Timezone(_) => write!(f, "Timezone"),
            Value::QuicSession(_) => write!(f, "QuicSession"),
            Value::Http2Session { base_url, .. } => write!(f, "Http2Session({base_url})"),
            Value::Http3Session(_) => write!(f, "Http3Session"),
            Value::QuicDatagramHandle(_) => write!(f, "QuicDatagramHandle"),
            Value::Program { .. } => write!(f, "Program(...)"),
            Value::Document(_) => write!(f, "Document(...)"),
            Value::Expression(node) => write!(
                f,
                "Expression({})",
                crate::surface_fields::surface_expr_tag(&node.expr)
            ),
            Value::Task(_) => write!(f, "Task"),
            Value::Channel(_) => write!(f, "Channel"),
            Value::Context(_) => write!(f, "Context"),
            Value::ReactiveCell(_) => write!(f, "ReactiveCell"),
            Value::BroadcastChannel(_) => write!(f, "BroadcastChannel"),
            Value::OneshotSender(_) => write!(f, "OneshotSender"),
            Value::OneshotReceiver(_) => write!(f, "OneshotReceiver"),
            Value::Arena { name, start_env_id } => write!(f, "Arena({name}@{start_env_id})"),
            Value::Annotated { inner, .. } => write!(f, "Annotated({inner:?})"),
            Value::TypeContext(_) => write!(f, "TypeContext"),
            Value::CoreDocument { entries, .. } => {
                write!(f, "CoreDocument({} entries)", entries.len())
            }
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{n}"),
            Value::U64(n) => write!(f, "{n}"),
            Value::Float(n) => write!(f, "{n}"),
            Value::String { source, start, end } => {
                let s = &source[*start..*end];
                write!(f, "{s:?}")
            }
            Value::Bool(b) => write!(f, "{b}"),
            Value::Dict(map) => {
                write!(f, "[")?;
                for (i, (key, _)) in map.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{key}: <thunk>")?;
                }
                write!(f, "]")
            }
            Value::Builder(builder) => {
                if builder.is_frozen() {
                    write!(f, "<Builder (frozen)>")
                } else {
                    write!(f, "<Builder>")
                }
            }
            Value::Function { params, .. } => {
                write!(f, "[fn [let")?;
                for p in params.iter() {
                    write!(f, " {}", p.name)?;
                }
                write!(f, "] ...]")
            }
            Value::Builtin(def) => write!(f, "<builtin {}>", def.name),
            Value::Seq { .. } => write!(f, "Seq(...)"),
            Value::Proxy { .. } => write!(f, "<proxy>"),
            Value::Overlay(..) => write!(f, "[<overlay>]"),
            Value::DirCap { .. } => write!(f, "<DirCap>"),
            Value::NetCap(_) => write!(f, "<NetCap>"),
            Value::File(_) => write!(f, "<File>"),
            Value::RevocableDirCap { revoked, .. } => {
                if revoked.load(Ordering::Acquire) {
                    write!(f, "<DirCap (revoked)>")
                } else {
                    write!(f, "<DirCap (revocable)>")
                }
            }
            Value::Variant {
                tycon,
                ctor,
                payload,
            } => {
                if payload.is_some() {
                    write!(f, "{}.{}(<payload>)", tycon, ctor)
                } else {
                    write!(f, "{}.{}", tycon, ctor)
                }
            }
            Value::Decimal(d) => write!(f, "{d}"),
            Value::BigInt(n) => write!(f, "{n}"),
            Value::Bytes { source, start, end } => {
                let bytes = &source[*start..*end];
                write!(f, "<bytes:{} bytes>", bytes.len())
            }
            Value::Uri { uri, .. } => write!(f, "{uri}"),
            Value::Timestamp(nanos) => match jiff::Timestamp::from_nanosecond(*nanos as i128) {
                Ok(ts) => write!(f, "{ts}"),
                Err(_) => write!(f, "<invalid timestamp>"),
            },
            Value::Duration(nanos) => {
                write!(f, "{nanos}ns")
            }
            Value::ClockCap(_) => write!(f, "<ClockCap>"),
            Value::Timezone(_) => write!(f, "<Timezone>"),
            Value::QuicSession(_) => write!(f, "<QuicSession>"),
            Value::Http2Session { base_url, .. } => write!(f, "<Http2Session {base_url}>"),
            Value::Http3Session(_) => write!(f, "<Http3Session>"),
            Value::QuicDatagramHandle(_) => write!(f, "<QuicDatagramHandle>"),
            Value::Program { .. } => write!(f, "<program>"),
            Value::Document(_) => write!(f, "<document>"),
            Value::Expression(node) => write!(
                f,
                "<expression:{}>",
                crate::surface_fields::surface_expr_tag(&node.expr)
            ),
            Value::Task(_) => write!(f, "<task>"),
            Value::Channel(_) => write!(f, "<channel>"),
            Value::Context(_) => write!(f, "<context>"),
            Value::ReactiveCell(_) => write!(f, "<reactive-cell>"),
            Value::BroadcastChannel(_) => write!(f, "<broadcast-channel>"),
            Value::OneshotSender(_) => write!(f, "<oneshot-sender>"),
            Value::OneshotReceiver(_) => write!(f, "<oneshot-receiver>"),
            Value::Arena { name, start_env_id } => write!(f, "<arena:{name}@{start_env_id}>"),
            Value::Annotated { inner, .. } => fmt::Display::fmt(inner, f),
            Value::TypeContext(_) => write!(f, "<TypeContext>"),
            Value::CoreDocument { entries, .. } => {
                write!(f, "<core-document:{} entries>", entries.len())
            }
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::U64(a), Value::U64(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (
                Value::String {
                    source: src_a,
                    start: start_a,
                    end: end_a,
                },
                Value::String {
                    source: src_b,
                    start: start_b,
                    end: end_b,
                },
            ) => src_a[*start_a..*end_a] == src_b[*start_b..*end_b],
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Decimal(a), Value::Decimal(b)) => a == b,
            (Value::BigInt(a), Value::BigInt(b)) => a == b,
            (
                Value::Bytes {
                    source: src_a,
                    start: start_a,
                    end: end_a,
                },
                Value::Bytes {
                    source: src_b,
                    start: start_b,
                    end: end_b,
                },
            ) => src_a[*start_a..*end_a] == src_b[*start_b..*end_b],
            (
                Value::Uri {
                    scheme: scheme_a,
                    uri: uri_a,
                },
                Value::Uri {
                    scheme: scheme_b,
                    uri: uri_b,
                },
            ) => scheme_a == scheme_b && uri_a == uri_b,
            (Value::Timestamp(a), Value::Timestamp(b)) => a == b,
            (Value::Duration(a), Value::Duration(b)) => a == b,
            (Value::ClockCap(a), Value::ClockCap(b)) => a == b,
            (Value::QuicSession(a), Value::QuicSession(b)) => Arc::ptr_eq(a, b),
            (Value::Http2Session { client: a, .. }, Value::Http2Session { client: b, .. }) => {
                Arc::ptr_eq(a, b)
            }
            (Value::Http3Session(a), Value::Http3Session(b)) => Arc::ptr_eq(a, b),
            (Value::QuicDatagramHandle(a), Value::QuicDatagramHandle(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

// ============================================================================
// Runtime v2 — Sprint 2B: ThunkInner + UnevaluatedState
// ============================================================================

/// Pre-evaluation state variants for the ThunkInner structure.
/// Stores the data needed to evaluate a thunk when it's first accessed.
#[derive(Debug, Clone)]
pub enum UnevaluatedState {
    /// Lazy AST node field access via `surface_node_get_field`.
    AstField {
        node: Arc<SurfaceNode>,
        field: &'static str,
        ctx: Arc<crate::eval::EvalContext>,
    },
    /// CoreExpr body thunk — created by invoke_function when body is Arc<Spanned<CoreExpr>>.
    CoreExpr {
        expr: Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
        /// Index into EvalContext.scope_arena (ScopeArena) for the evaluation environment.
        env_id: u32,
        ctx: Arc<crate::eval::EvalContext>,
    },
    /// Deferred builtin call (was PendingBuiltin).
    BuiltinCall {
        def: BuiltinDef,
        args: Vec<ThunkId>,
        named: Option<IndexMap<String, ThunkId>>,
        call_span: Span,
        /// Index into EvalContext.scope_arena (ScopeArena) for the caller's environment.
        caller_env_id: u32,
        ctx: Arc<crate::eval::EvalContext>,
    },
    /// Deferred function call (was PendingCall).
    FnCall {
        func: ThunkId,
        args: Vec<ThunkId>,
        named: Option<Box<IndexMap<String, ThunkId>>>,
        call_span: Span,
        /// Index into EvalContext.scope_arena (ScopeArena) for the caller's environment.
        caller_env_id: u32,
        ctx: Arc<crate::eval::EvalContext>,
        /// Original CoreExpr::Call node for DepthExceeded retry path.
        original_call: Arc<Spanned<CoreExpr>>,
    },
    /// Type guard wrapping an inner thunk (was Guarded).
    Guarded {
        inner: ThunkId,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
        blame_label: Option<crate::error::BlameLabel>,
        default: Option<GuardDefault>,
    },
    /// Deferred Value::Annotated wrapper — forces inner thunk then wraps in Value::Annotated.
    ///
    /// Created by eval_dict_core (T-1621) when a dict-key annotation's value is a non-literal
    /// thunk. When materialized, forces `inner` and produces
    /// `Value::Annotated { inner: forced_inner, annotation }`.
    AnnotatedWrap {
        inner: ThunkId,
        annotation: Box<Value>,
        ctx: Arc<crate::eval::EvalContext>,
    },
}

impl UnevaluatedState {
    pub fn initial_env_id(&self) -> u32 {
        match self {
            UnevaluatedState::CoreExpr { env_id, .. } => *env_id,
            UnevaluatedState::BuiltinCall { caller_env_id, .. } => *caller_env_id,
            UnevaluatedState::FnCall { caller_env_id, .. } => *caller_env_id,
            UnevaluatedState::AstField { .. } => 0,
            UnevaluatedState::Guarded { .. } => 0,
            UnevaluatedState::AnnotatedWrap { .. } => 0,
        }
    }
}

/// New thunk structure for async evaluation (Sprint 2B).
/// Replaces Mutex<ThunkState> with a two-field pair:
/// - unevaluated: taken (set to None) when evaluation starts
/// - result: set exactly once when evaluation completes
#[derive(Debug)]
pub struct ThunkInner {
    /// Combined: (UnevaluatedState, evaluating_task_id).
    /// Both fields transition atomically in try_claim().
    pub unevaluated: Mutex<(Option<UnevaluatedState>, Option<tokio::task::Id>)>,

    /// Terminal result. Set exactly once (OnceCell).
    pub result: tokio::sync::OnceCell<Result<Value, Arc<EvalError>>>,

    /// Resolves when result is set. Allows tasks to await settlement.
    pub notify: Arc<tokio::sync::Notify>,
}

#[derive(Debug)]
pub enum ThunkState {
    Unevaluated,
    InProgress {
        evaluating_task: Option<tokio::task::Id>,
    },
    Materialized(Value),
    Failed(Arc<EvalError>),
}

/// Lazy evaluation cell: wraps an unevaluated expression, a pending builtin call,
/// or a materialized value with memoization (evaluate-at-most-once semantics).
pub struct Thunk {
    inner: ThunkInner,
    pub(crate) span: Span,
}

pub(crate) struct ThunkPanicGuard(pub(crate) Option<Arc<Thunk>>);

impl ThunkPanicGuard {
    pub(crate) fn settle(mut self, result: Result<Value, Arc<EvalError>>) {
        let thunk = self.0.take().unwrap();
        thunk.settle(result);
    }
}

impl Drop for ThunkPanicGuard {
    fn drop(&mut self) {
        if let Some(thunk) = self.0.take() {
            thunk.settle(Err(Arc::new(EvalError::internal(
                "thunk evaluation task panicked".to_string(),
                thunk.span.clone(),
            ))));
        }
    }
}

impl Thunk {
    /// Create a placeholder thunk for letrec pre-allocation.
    pub fn placeholder(span: Span) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new((None, None)),
                result: tokio::sync::OnceCell::new(),
                notify: Arc::new(tokio::sync::Notify::new()),
            },
            span,
        }
    }

    /// Create an unevaluated thunk from a CoreExpr body (no Expr round-trip).
    pub fn core_expr(
        expr: Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
        env_id: u32,
        ctx: Arc<crate::eval::EvalContext>,
        span: Span,
    ) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new((
                    Some(UnevaluatedState::CoreExpr { expr, env_id, ctx }),
                    None,
                )),
                result: tokio::sync::OnceCell::new(),
                notify: Arc::new(tokio::sync::Notify::new()),
            },
            span,
        }
    }

    pub fn value(value: Value, span: Span) -> Self {
        let inner = ThunkInner {
            unevaluated: Mutex::new((None, None)),
            result: tokio::sync::OnceCell::new(),
            notify: Arc::new(tokio::sync::Notify::new()),
        };
        let _ = inner.result.set(Ok(value));
        Self { inner, span }
    }

    /// Create a lazy AstField thunk.
    pub fn ast_field(
        node: std::sync::Arc<crate::ast::SurfaceNode>,
        field: &'static str,
        ctx: Arc<crate::eval::EvalContext>,
        span: Span,
    ) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new((
                    Some(UnevaluatedState::AstField { node, field, ctx }),
                    None,
                )),
                result: tokio::sync::OnceCell::new(),
                notify: Arc::new(tokio::sync::Notify::new()),
            },
            span,
        }
    }

    /// Create a deferred Value::Annotated thunk (T-1621).
    ///
    /// When forced, materializes `inner` and produces
    /// `Value::Annotated { inner: forced_inner, annotation }`.
    pub fn annotated_wrap(
        inner: ThunkId,
        annotation: Value,
        ctx: Arc<crate::eval::EvalContext>,
        span: Span,
    ) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new((
                    Some(UnevaluatedState::AnnotatedWrap {
                        inner,
                        annotation: Box::new(annotation),
                        ctx,
                    }),
                    None,
                )),
                result: tokio::sync::OnceCell::new(),
                notify: Arc::new(tokio::sync::Notify::new()),
            },
            span,
        }
    }

    pub fn builtin_call(
        def: BuiltinDef,
        args: Vec<ThunkId>,
        named: Option<IndexMap<String, ThunkId>>,
        span: Span,
        caller_env_id: u32,
        ctx: Arc<crate::eval::EvalContext>,
    ) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new((
                    Some(UnevaluatedState::BuiltinCall {
                        def,
                        args,
                        named,
                        call_span: span.clone(),
                        caller_env_id,
                        ctx,
                    }),
                    None,
                )),
                result: tokio::sync::OnceCell::new(),
                notify: Arc::new(tokio::sync::Notify::new()),
            },
            span,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn fn_call(
        func: ThunkId,
        args: Vec<ThunkId>,
        named: IndexMap<String, ThunkId>,
        call_span: Span,
        caller_env_id: u32,
        span: Span,
        ctx: Arc<crate::eval::EvalContext>,
        original_call: Arc<Spanned<CoreExpr>>,
    ) -> Self {
        let named_opt = if named.is_empty() {
            None
        } else {
            Some(Box::new(named))
        };
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new((
                    Some(UnevaluatedState::FnCall {
                        func,
                        args,
                        named: named_opt,
                        call_span,
                        caller_env_id,
                        ctx,
                        original_call,
                    }),
                    None,
                )),
                result: tokio::sync::OnceCell::new(),
                notify: Arc::new(tokio::sync::Notify::new()),
            },
            span,
        }
    }

    pub fn guarded(
        inner: ThunkId,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
        blame_label: Option<crate::error::BlameLabel>,
        default: Option<GuardDefault>,
    ) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new((
                    Some(UnevaluatedState::Guarded {
                        inner,
                        expected,
                        field_path,
                        guard_span: guard_span.clone(),
                        blame_label,
                        default,
                    }),
                    None,
                )),
                result: tokio::sync::OnceCell::new(),
                notify: Arc::new(tokio::sync::Notify::new()),
            },
            span: guard_span.with_name(Arc::from("type guard")),
        }
    }

    /// Return the source span where this thunk was created.
    pub fn definition_span(&self) -> Span {
        self.span.clone()
    }

    pub fn state(&self) -> ThunkState {
        if let Some(result) = self.inner.result.get() {
            return match result {
                Ok(v) => ThunkState::Materialized(v.clone()),
                Err(e) => ThunkState::Failed(Arc::clone(e)),
            };
        }
        let guard = self.inner.unevaluated.lock().unwrap();
        match &guard.0 {
            Some(_) => ThunkState::Unevaluated,
            None => ThunkState::InProgress {
                evaluating_task: guard.1,
            },
        }
    }

    pub fn settle(&self, result: Result<Value, Arc<EvalError>>) {
        let _ = self.inner.result.set(result);
        {
            let mut guard = self.inner.unevaluated.lock().unwrap();
            guard.1 = None;
        }
        self.inner.notify.notify_waiters();
    }

    pub async fn settled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.result.get().is_some() {
                return;
            }
            notified.await;
            if self.inner.result.get().is_some() {
                return;
            }
        }
    }

    pub fn try_claim(&self) -> Option<UnevaluatedState> {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        let (state, task_id) = &mut *guard;
        let taken = state.take()?;
        *task_id = tokio::task::try_id();
        Some(taken)
    }

    pub fn reset(&self, state: UnevaluatedState) {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        guard.0 = Some(state);
        guard.1 = None;
    }

    /// Create a new `Arc<Thunk>` that is identical to `self` but with `new_ctx` replacing
    /// the birth context in the unevaluated state.
    pub(crate) fn with_replaced_ctx(
        &self,
        new_ctx: Arc<crate::eval::EvalContext>,
    ) -> Option<Arc<Thunk>> {
        let guard = self.inner.unevaluated.lock().unwrap();
        let state = match &guard.0 {
            None => return None,
            Some(s) => s.clone(),
        };
        drop(guard);
        let new_state = match state {
            UnevaluatedState::CoreExpr {
                expr,
                env_id,
                ctx: _,
            } => UnevaluatedState::CoreExpr {
                expr,
                env_id,
                ctx: new_ctx,
            },
            UnevaluatedState::AstField {
                node,
                field,
                ctx: _,
            } => UnevaluatedState::AstField {
                node,
                field,
                ctx: new_ctx,
            },
            UnevaluatedState::BuiltinCall {
                def,
                args,
                named,
                call_span,
                caller_env_id,
                ctx: _,
            } => UnevaluatedState::BuiltinCall {
                def,
                args,
                named,
                call_span,
                caller_env_id,
                ctx: new_ctx,
            },
            UnevaluatedState::FnCall {
                func,
                args,
                named,
                call_span,
                caller_env_id,
                ctx: _,
                original_call,
            } => UnevaluatedState::FnCall {
                func,
                args,
                named,
                call_span,
                caller_env_id,
                ctx: new_ctx,
                original_call,
            },
            UnevaluatedState::Guarded {
                inner,
                expected,
                field_path,
                guard_span,
                blame_label,
                default,
            } => UnevaluatedState::Guarded {
                inner,
                expected,
                field_path,
                guard_span,
                blame_label,
                default,
            },
            UnevaluatedState::AnnotatedWrap {
                inner,
                annotation,
                ctx: _,
            } => UnevaluatedState::AnnotatedWrap {
                inner,
                annotation,
                ctx: new_ctx,
            },
        };
        Some(Arc::new(Thunk {
            inner: ThunkInner {
                unevaluated: Mutex::new((Some(new_state), None)),
                result: tokio::sync::OnceCell::new(),
                notify: Arc::new(tokio::sync::Notify::new()),
            },
            span: self.span.clone(),
        }))
    }

    pub fn try_get_materialized(&self) -> Option<Value> {
        match self.state() {
            ThunkState::Materialized(v) => Some(v),
            _ => None,
        }
    }

    pub fn get_cached_error(&self) -> Option<Box<EvalError>> {
        match self.state() {
            ThunkState::Failed(e) => Some(Box::new((*e).clone())),
            _ => None,
        }
    }

    pub fn is_materialized(&self) -> bool {
        matches!(self.state(), ThunkState::Materialized(_))
    }

    // ========================================================================
    // Non-destructive introspection methods
    // ========================================================================

    /// Peek at the builtin def if this thunk is in Unevaluated BuiltinCall state.
    pub fn peek_builtin_def(&self) -> Option<BuiltinDef> {
        let guard = self.inner.unevaluated.lock().unwrap();
        match &guard.0 {
            Some(UnevaluatedState::BuiltinCall { def, .. }) => Some(*def),
            _ => None,
        }
    }

    /// Check if this thunk is in Guarded state.
    pub fn is_guarded(&self) -> bool {
        let guard = self.inner.unevaluated.lock().unwrap();
        matches!(&guard.0, Some(UnevaluatedState::Guarded { .. }))
    }

    /// Check if this thunk is in FnCall state.
    pub fn is_pending_call(&self) -> bool {
        let guard = self.inner.unevaluated.lock().unwrap();
        matches!(&guard.0, Some(UnevaluatedState::FnCall { .. }))
    }

    /// Peek at the AstField if this thunk is in AstField state.
    pub fn peek_ast_node_field(
        &self,
    ) -> Option<(std::sync::Arc<crate::ast::SurfaceNode>, &'static str)> {
        let guard = self.inner.unevaluated.lock().unwrap();
        match &guard.0 {
            Some(UnevaluatedState::AstField { node, field, .. }) => {
                Some((std::sync::Arc::clone(node), *field))
            }
            _ => None,
        }
    }
}

impl fmt::Debug for Thunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("Thunk");

        match self.inner.unevaluated.try_lock() {
            Ok(guard) => {
                if let Some(result) = self.inner.result.get() {
                    match result {
                        Ok(value) => {
                            s.field("state", &format!("Materialized({:?})", value.type_name()))
                        }
                        Err(_) => s.field("state", &"Failed"),
                    };
                } else if guard.0.is_some() {
                    s.field("state", &"Unevaluated");
                } else {
                    s.field("state", &"InProgress");
                }
            }
            Err(_) => {
                s.field("state", &"<locked>");
            }
        }

        s.field("span", &self.span);
        if let Some(ref name) = self.span.name {
            s.field("name", name);
        }
        s.finish()
    }
}

/// Lexical scope chain: bindings in the current scope plus an optional parent link.
/// Currently used only as a placeholder in match dispatch (B-515 tracks FlatEnv migration).
/// Fields and methods are retained for B-515 wiring.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Environment {
    pub(crate) bindings: IndexMap<String, Arc<Thunk>>,
    pub(crate) parent: Option<Arc<RwLock<Environment>>>,
}

/// Profiling counters for slot-based lookup hit rate measurement.
#[cfg(test)]
pub(crate) static SLOT_HIT_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) static SLOT_MISS_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Reset profiling counters between tests.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn reset_slot_counters() {
    use std::sync::atomic::Ordering;
    SLOT_HIT_COUNT.store(0, Ordering::Relaxed);
    SLOT_MISS_COUNT.store(0, Ordering::Relaxed);
}

#[allow(dead_code)]
impl Environment {
    pub fn new() -> Self {
        Self {
            bindings: IndexMap::new(),
            parent: None,
        }
    }

    pub fn with_parent(parent: Arc<RwLock<Environment>>) -> Self {
        Self {
            bindings: IndexMap::new(),
            parent: Some(parent),
        }
    }

    /// Look up a binding by name, searching this environment then ancestors.
    pub fn get(&self, name: &str) -> Option<Arc<Thunk>> {
        if let Some(thunk) = self.bindings.get(name) {
            return Some(Arc::clone(thunk));
        }
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_rc) = current {
            let env = env_rc.read().unwrap();
            if let Some(thunk) = env.bindings.get(name) {
                return Some(Arc::clone(thunk));
            }
            current = env.parent.as_ref().map(Arc::clone);
        }
        None
    }

    pub fn insert(&mut self, name: String, thunk: Arc<Thunk>) {
        self.bindings.insert(name, thunk);
    }

    /// O(1) slot-based lookup with De Bruijn level-based parent chain walking.
    pub fn get_by_slot(&self, level: u32, slot: u32, expected_name: &str) -> Option<Arc<Thunk>> {
        if level == 0 {
            if let Some((key, thunk)) = self.bindings.get_index(slot as usize) {
                if key == expected_name {
                    #[cfg(test)]
                    SLOT_HIT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Some(Arc::clone(thunk));
                } else {
                    #[cfg(test)]
                    SLOT_MISS_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return self.bindings.get(expected_name).map(Arc::clone);
                }
            }
            #[cfg(test)]
            SLOT_MISS_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return None;
        }
        let mut steps_remaining = level;
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_rc) = current {
            steps_remaining -= 1;
            if steps_remaining == 0 {
                let env = env_rc.read().unwrap();
                if let Some((key, thunk)) = env.bindings.get_index(slot as usize) {
                    if key == expected_name {
                        #[cfg(test)]
                        SLOT_HIT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return Some(Arc::clone(thunk));
                    } else {
                        #[cfg(test)]
                        SLOT_MISS_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return env.bindings.get(expected_name).map(Arc::clone);
                    }
                }
                #[cfg(test)]
                SLOT_MISS_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return None;
            }
            let next = env_rc.read().unwrap().parent.as_ref().map(Arc::clone);
            current = next;
        }
        #[cfg(test)]
        SLOT_MISS_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        None
    }
}

impl Default for Environment {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{CoreExpr, Spanned};
    use crate::test_util::test_span;

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        crate::eval::EvalContext::new(base_dir, false)
    }

    #[test]
    fn test_state_of_unevaluated() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);
        let expr = Arc::new(Spanned::new(CoreExpr::Int(42), span.clone()));
        let thunk = Thunk::core_expr(expr, 0, ctx, span);

        match thunk.state() {
            ThunkState::Unevaluated => {}
            other => panic!("Expected Unevaluated, got {:?}", other),
        }
    }

    #[test]
    fn test_state_of_materialized() {
        let span = test_span(1, 1, 1, 10);
        let thunk = Thunk::value(Value::Int(42), span);

        match thunk.state() {
            ThunkState::Materialized(Value::Int(42)) => {}
            other => panic!("Expected Materialized(Int(42)), got {:?}", other),
        }
    }

    #[test]
    fn test_settle_ok() {
        let span = test_span(1, 1, 1, 10);
        let thunk = Thunk::placeholder(span);

        thunk.settle(Ok(Value::Int(1)));

        match thunk.state() {
            ThunkState::Materialized(Value::Int(1)) => {}
            other => panic!("Expected Materialized(Int(1)), got {:?}", other),
        }
    }

    #[test]
    fn test_settle_err() {
        let span = test_span(1, 1, 1, 10);
        let thunk = Thunk::placeholder(span.clone());

        let error = Arc::new(crate::error::EvalError::internal(
            "test error".to_string(),
            span,
        ));
        thunk.settle(Err(Arc::clone(&error)));

        match thunk.state() {
            ThunkState::Failed(e) => {
                assert_eq!(Arc::as_ptr(&e), Arc::as_ptr(&error));
            }
            other => panic!("Expected Failed, got {:?}", other),
        }
    }

    #[test]
    fn test_settle_idempotent() {
        let span = test_span(1, 1, 1, 10);
        let thunk = Thunk::placeholder(span);

        thunk.settle(Ok(Value::Int(1)));
        // Second settle should be no-op (OnceCell ignores duplicate set)
        thunk.settle(Ok(Value::Int(999)));

        match thunk.state() {
            ThunkState::Materialized(Value::Int(1)) => {}
            other => panic!("Expected Materialized(Int(1)), got {:?}", other),
        }
    }

    #[test]
    fn test_try_claim_transitions_to_inprogress() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);
        let expr = Arc::new(Spanned::new(CoreExpr::Int(42), span.clone()));
        let thunk = Thunk::core_expr(expr.clone(), 0, ctx.clone(), span);

        let state = thunk.try_claim();
        assert!(state.is_some(), "try_claim should succeed on unevaluated");

        // Verify the returned state is CoreExpr
        match state.unwrap() {
            UnevaluatedState::CoreExpr { expr: e, .. } => {
                assert_eq!(Arc::as_ptr(&e), Arc::as_ptr(&expr));
            }
            other => panic!("Expected CoreExpr state, got {:?}", other),
        }

        // Verify thunk is now InProgress
        match thunk.state() {
            ThunkState::InProgress { .. } => {}
            other => panic!("Expected InProgress, got {:?}", other),
        }
    }

    #[test]
    fn test_try_claim_returns_none_when_inprogress() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);
        let expr = Arc::new(Spanned::new(CoreExpr::Int(42), span.clone()));
        let thunk = Thunk::core_expr(expr, 0, ctx, span);

        let first_claim = thunk.try_claim();
        assert!(first_claim.is_some(), "First claim should succeed");

        let second_claim = thunk.try_claim();
        assert!(second_claim.is_none(), "Second claim should return None");
    }

    #[test]
    fn test_reset_restores_unevaluated() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);
        let expr = Arc::new(Spanned::new(CoreExpr::Int(42), span.clone()));
        let thunk = Thunk::core_expr(expr, 0, ctx.clone(), span);

        let state = thunk.try_claim().expect("try_claim should succeed");

        // Verify InProgress
        match thunk.state() {
            ThunkState::InProgress { .. } => {}
            other => panic!("Expected InProgress after claim, got {:?}", other),
        }

        thunk.reset(state);

        // Verify restored to Unevaluated
        match thunk.state() {
            ThunkState::Unevaluated => {}
            other => panic!("Expected Unevaluated after reset, got {:?}", other),
        }
    }

    #[test]
    fn test_try_get_materialized_convenience() {
        let span = test_span(1, 1, 1, 10);
        let thunk = Thunk::value(Value::Int(42), span);

        match thunk.try_get_materialized() {
            Some(Value::Int(42)) => {}
            other => panic!("Expected Some(Int(42)), got {:?}", other),
        }
    }

    #[test]
    fn test_get_cached_error_convenience() {
        let span = test_span(1, 1, 1, 10);
        let thunk = Thunk::placeholder(span.clone());

        let error = Arc::new(crate::error::EvalError::internal(
            "test error".to_string(),
            span,
        ));
        thunk.settle(Err(Arc::clone(&error)));

        let cached = thunk.get_cached_error();
        assert!(cached.is_some(), "get_cached_error should return Some");
        assert_eq!(cached.unwrap().kind.to_string(), error.kind.to_string());
    }
}
