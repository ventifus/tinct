//! Runtime value types: `Value`, `Thunk` (lazy memoization), `Environment` (legacy name chain), `Scope` (runtime scope via `ScopeArena`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::rc::Rc;
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
    /// Caller's scope id — enables scope-based variable lookup in builtins.
    /// Copied from UnevaluatedState::Builtin.caller_env_id at materialization time.
    pub caller_env_id: u32,
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
            .finish_non_exhaustive()
    }
}

/// Dict key type: either an integer (auto-indexed) or a string (bare word / quoted).
/// This is the canonical hashable key used in `Value::Dict` and `IndexMap<HashableValue, ThunkId>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HashableValue {
    Int(i64),
    Str(Rc<str>),
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
        // literal without allocating a temporary Rc<str>.
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
        // Using 2u8 directly avoids the Rc::from("") allocation that the
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
    inner: Mutex<Option<IndexMap<HashableValue, ThunkId>>>,
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
    pub fn set(&self, key: HashableValue, value: ThunkId) -> Result<(), String> {
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
    pub fn get(&self, key: &HashableValue) -> Option<ThunkId> {
        // Fast-path: frozen builder has no entries.
        if self.frozen.load(Ordering::Relaxed) {
            return None;
        }
        let guard = self.inner.lock().unwrap();
        guard.as_ref().and_then(|map| map.get(key).copied())
    }

    /// Atomically get-or-insert: if `key` exists, return its ThunkId; otherwise
    /// insert `default_id` at `key` and return `default_id`.
    /// Returns error if the builder is frozen.
    ///
    /// This eliminates the `builder-has?` + `builder-get` + `builder-set` triple
    /// that `group-by` previously used, reducing locking overhead and avoiding the
    /// race window between the has? check and the set.
    pub fn get_or(&self, key: HashableValue, default_id: ThunkId) -> Result<ThunkId, String> {
        // Fast-path: frozen builder cannot be mutated.
        if self.frozen.load(Ordering::Relaxed) {
            return Err("builder is frozen (already finished)".to_string());
        }
        let mut guard = self.inner.lock().unwrap();
        match guard.as_mut() {
            Some(map) => {
                if let Some(&existing) = map.get(&key) {
                    Ok(existing)
                } else {
                    map.insert(key, default_id);
                    Ok(default_id)
                }
            }
            None => Err("builder is frozen (already finished)".to_string()),
        }
    }

    /// Take the inner map, freezing the builder. Returns error if already frozen.
    pub fn finish(&self) -> Result<IndexMap<HashableValue, ThunkId>, String> {
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
    pub fn snapshot(&self) -> Result<IndexMap<HashableValue, ThunkId>, String> {
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
#[derive(Clone)]
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
        source: Rc<str>,
        start: usize,
        end: usize,
    },
    /// Boolean (`true` or `false`)
    Bool(bool),
    /// Ordered key-value map with lazy (thunked) values
    Dict(IndexMap<HashableValue, ThunkId>),
    /// Transient builder for efficient mutable dict construction.
    /// One-shot invariant: once frozen (via builder-finish), all mutations error.
    /// Sequential-use: not safe for concurrent modification (Mutex protects state, not semantics).
    Builder(Arc<Builder>),
    /// User-defined function (closure capturing its defining environment).
    /// `body` is stored as `Arc<Spanned<CoreExpr>>` (Parts-E migration: no Expr round-trip).
    /// `closure_env_id` is the ScopeId index into EvalContext.scope_arena for the closure scope.
    Function {
        params: Rc<Vec<Param>>,
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
        dir: Rc<cap_std::fs::Dir>,
        perms: DirPerms,
    },
    /// Network capability — authority to connect to specified hosts/subnets
    NetCap(Rc<Vec<NetCapEntry>>),
    /// Open file/stream handle with capability metadata.
    Handle {
        caps: HashMap<String, Value>,
        inner: Rc<std::cell::RefCell<Box<dyn std::io::BufRead>>>,
        write_inner: Option<Rc<std::cell::RefCell<Box<dyn std::io::Write>>>>,
        seek_inner: Option<Rc<std::cell::RefCell<Box<dyn std::io::Seek>>>>,
        raw_tcp: Option<Rc<RefCell<Option<std::net::TcpStream>>>>,
        creation_span: Span,
    },
    /// Write-only file/stream handle with capability metadata.
    WriteHandle {
        caps: HashMap<String, Value>,
        inner: Rc<std::cell::RefCell<Box<dyn std::io::Write>>>,
    },
    /// Raw OS file handle (thin wrapper over cap_std::fs::File, no buffering).
    /// Opened via `builtin-file-open`; read/written/sought via `builtin-file-*` builtins.
    File(Rc<std::cell::RefCell<cap_std::fs::File>>),
    /// Revocable directory capability
    RevocableDirCap {
        inner: Rc<cap_std::fs::Dir>,
        perms: DirPerms,
        revoked: Rc<std::cell::Cell<bool>>,
    },
    /// Nominal variant (enum-like value)
    Variant {
        tag: String,
        payload: Option<ThunkId>,
    },
    /// Exact base-10 decimal (rust_decimal::Decimal, 96-bit software decimal).
    Decimal(rust_decimal::Decimal),
    /// Arbitrary-precision integer (num_bigint::BigInt).
    BigInt(num_bigint::BigInt),
    /// Byte sequence (opaque binary data).
    Bytes {
        source: Rc<[u8]>,
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
    ClockCap(Rc<ClockCapInner>),
    /// Timezone (parsed IANA TZ rules from zoneinfo file).
    Timezone(Rc<jiff::tz::TimeZone>),
    /// QUIC session — multiplexed connection over UDP (RFC 9000).
    QuicSession(Rc<quinn::Connection>),
    /// HTTP/2 session — multiplexed HTTP connection (RFC 9113).
    Http2Session {
        client: Rc<reqwest::Client>,
        base_url: String,
    },
    /// HTTP/3 session — HTTP over QUIC (RFC 9114).
    Http3Session(Rc<RefCell<Http3SessionState>>),
    /// QUIC datagram handle — unreliable message delivery over QUIC (RFC 9221).
    QuicDatagramHandle(Rc<quinn::Connection>),
    /// Message-oriented datagram socket (UDP or Unix datagram).
    DatagramHandle {
        socket: DatagramSocket,
        creation_span: Span,
    },

    // =========================================================================
    // runtime-v2 native AST value types
    // =========================================================================
    /// A complete tinct program — the type returned by `load` and `expand`.
    ///
    /// `id` is an index into `EvalContext.program_store` (a `Vec<SurfaceProgram>`).
    /// Carrying an id instead of `Arc<SurfaceProgram>` means that `builtin_desugar` can
    /// mutate the program in-place (via `with_program_mut`) without needing ownership or
    /// deep-cloning to get a unique reference. Consistent with the arena pattern: ThunkId
    /// is a coordinate, not data — programs are the same.
    Program {
        id: u32,
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

/// Socket variant carried inside `Value::DatagramHandle`.
#[derive(Clone, Debug)]
pub enum DatagramSocket {
    Udp(Rc<RefCell<std::net::UdpSocket>>),
    #[cfg(unix)]
    UnixDgram(Rc<RefCell<std::os::unix::net::UnixDatagram>>),
}

/// Helper function to construct a `Value::String` from a string slice.
pub fn string_val(s: &str) -> Value {
    Value::String {
        source: Rc::from(s),
        start: 0,
        end: s.len(),
    }
}

/// Helper function to construct a `Value::Bytes` from a byte slice.
pub fn bytes_val(data: &[u8]) -> Value {
    Value::Bytes {
        source: Rc::from(data),
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
            Value::Handle { .. } => "Handle",
            Value::WriteHandle { .. } => "WriteHandle",
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
            Value::DatagramHandle { .. } => "DatagramHandle",
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
            Value::Handle { caps, .. } => write!(f, "Handle({} caps)", caps.len()),
            Value::WriteHandle { caps, .. } => write!(f, "WriteHandle({} caps)", caps.len()),
            Value::File(_) => write!(f, "File"),
            Value::RevocableDirCap { revoked, .. } => {
                if revoked.get() {
                    write!(f, "DirCap(revoked)")
                } else {
                    write!(f, "DirCap(revocable)")
                }
            }
            Value::Variant { tag, payload } => {
                if payload.is_some() {
                    write!(f, "Variant({tag}, <payload>)")
                } else {
                    write!(f, "Variant({tag})")
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
            Value::DatagramHandle { .. } => write!(f, "DatagramHandle"),
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
            Value::Handle { .. } => write!(f, "<Handle>"),
            Value::WriteHandle { .. } => write!(f, "<WriteHandle>"),
            Value::File(_) => write!(f, "<File>"),
            Value::RevocableDirCap { revoked, .. } => {
                if revoked.get() {
                    write!(f, "<DirCap (revoked)>")
                } else {
                    write!(f, "<DirCap (revocable)>")
                }
            }
            Value::Variant { tag, payload } => {
                if payload.is_some() {
                    write!(f, "{tag}(<payload>)")
                } else {
                    write!(f, "{tag}")
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
            Value::DatagramHandle { .. } => write!(f, "<DatagramHandle>"),
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
            (Value::QuicSession(a), Value::QuicSession(b)) => Rc::ptr_eq(a, b),
            (Value::Http2Session { client: a, .. }, Value::Http2Session { client: b, .. }) => {
                Rc::ptr_eq(a, b)
            }
            (Value::Http3Session(a), Value::Http3Session(b)) => Rc::ptr_eq(a, b),
            (Value::QuicDatagramHandle(a), Value::QuicDatagramHandle(b)) => Rc::ptr_eq(a, b),
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
    /// Pre-lowering Surface thunk — created by the `eval` builtin.
    Surface {
        node: Arc<SurfaceNode>,
        res: Arc<crate::ast::ResolutionTable>,
        types: Arc<crate::ast::TypeAnnotationTable>,
        /// Index into EvalContext.scope_arena (ScopeArena) for the evaluation environment.
        env_id: u32,
        ctx: Arc<crate::eval::EvalContext>,
    },
    /// Lazy AST node field access via `surface_node_get_field`.
    AstNodeField {
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
    Builtin {
        def: BuiltinDef,
        args: Vec<ThunkId>,
        named: Option<IndexMap<String, ThunkId>>,
        call_span: Span,
        /// Index into EvalContext.scope_arena (ScopeArena) for the caller's environment.
        caller_env_id: u32,
        ctx: Arc<crate::eval::EvalContext>,
    },
    /// Deferred function call (was PendingCall).
    Call {
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
}

/// New thunk structure for async evaluation (Sprint 2B).
/// Replaces Mutex<ThunkState> with a two-field pair:
/// - unevaluated: taken (set to None) when evaluation starts
/// - result: set exactly once when evaluation completes
#[derive(Debug)]
pub struct ThunkInner {
    /// Pre-evaluation state. Set to Some initially, taken (set to None) when evaluation starts.
    pub unevaluated: Mutex<Option<UnevaluatedState>>,

    /// Post-evaluation result. Set exactly once when evaluation completes (success or failure).
    pub result: tokio::sync::OnceCell<Result<Value, Arc<EvalError>>>,
}

/// Lazy evaluation cell: wraps an unevaluated expression, a pending builtin call,
/// or a materialized value with memoization (evaluate-at-most-once semantics).
pub struct Thunk {
    inner: ThunkInner,
    pub(crate) span: Span,
    pub(crate) origin: Option<Arc<str>>,
    pub(crate) create_parent: Option<u64>,
    pub(crate) create_time_us: u64,
}

/// Return type of `Thunk::take_pending_builtin`.
pub type PendingBuiltinParts = (
    BuiltinDef,
    Vec<ThunkId>,
    Option<IndexMap<String, ThunkId>>,
    Span,
    u32, // caller_env_id
    Arc<crate::eval::EvalContext>,
);

/// Return type of `Thunk::take_pending_call`.
pub type PendingCallParts = (
    ThunkId, // func
    Vec<ThunkId>,
    Option<IndexMap<String, ThunkId>>,
    Span,
    u32, // caller_env_id
    Arc<crate::eval::EvalContext>,
    Arc<Spanned<CoreExpr>>,
);

/// Return type of `Thunk::take_core_expr`.
pub type CoreExprParts = (
    Arc<Spanned<CoreExpr>>,
    u32, // env_id
    Arc<crate::eval::EvalContext>,
);

/// Return type of `Thunk::take_guarded`.
pub type GuardedParts = (
    ThunkId, // inner
    Type,
    Vec<String>,
    Span,
    Option<crate::error::BlameLabel>,
    Option<(Arc<Spanned<CoreExpr>>, u32)>, // default: (expr, env_id)
);

/// Return type of `Thunk::take_surface`.
pub type SurfaceParts = (
    Arc<SurfaceNode>,
    Arc<crate::ast::ResolutionTable>,
    Arc<crate::ast::TypeAnnotationTable>,
    u32, // env_id
    Arc<crate::eval::EvalContext>,
);

impl Thunk {
    /// Helper: extract profiling data (create_parent, create_time_us) from context.
    fn profiling_data(ctx: &Arc<crate::eval::EvalContext>) -> (Option<u64>, u64) {
        if let Some(ref profiling) = ctx.profiling {
            let guard = profiling.lock().unwrap();
            let baseline = guard.baseline_instant();
            (
                guard.current_span_id(),
                baseline.elapsed().as_micros() as u64,
            )
        } else {
            (None, 0)
        }
    }

    /// Create a placeholder thunk for letrec pre-allocation.
    pub fn new_placeholder(span: Span) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(None),
                result: tokio::sync::OnceCell::new(),
            },
            span,
            origin: None,
            create_parent: None,
            create_time_us: 0,
        }
    }

    /// Create an unevaluated thunk from a CoreExpr body (no Expr round-trip).
    pub fn new_unevaluated_core(
        expr: Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
        env_id: u32,
        ctx: Arc<crate::eval::EvalContext>,
        span: Span,
    ) -> Self {
        let (create_parent, create_time_us) = Self::profiling_data(&ctx);
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(UnevaluatedState::CoreExpr { expr, env_id, ctx })),
                result: tokio::sync::OnceCell::new(),
            },
            span,
            origin: None,
            create_parent,
            create_time_us,
        }
    }

    pub fn new_materialized(value: Value, span: Span) -> Self {
        let inner = ThunkInner {
            unevaluated: Mutex::new(None),
            result: tokio::sync::OnceCell::new(),
        };
        let _ = inner.result.set(Ok(value));
        Self {
            inner,
            span,
            origin: None,
            create_parent: None,
            create_time_us: 0,
        }
    }

    /// Create a Surface thunk — wraps a SurfaceNode for lazy evaluation.
    pub fn new_surface(
        node: std::sync::Arc<crate::ast::SurfaceNode>,
        res: std::sync::Arc<crate::ast::ResolutionTable>,
        types: std::sync::Arc<crate::ast::TypeAnnotationTable>,
        env_id: u32,
        ctx: Arc<crate::eval::EvalContext>,
        span: Span,
    ) -> Self {
        let (create_parent, create_time_us) = Self::profiling_data(&ctx);
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(UnevaluatedState::Surface {
                    node,
                    res,
                    types,
                    env_id,
                    ctx,
                })),
                result: tokio::sync::OnceCell::new(),
            },
            span,
            origin: None,
            create_parent,
            create_time_us,
        }
    }

    /// Create a lazy AstNodeField thunk.
    pub fn new_ast_node_field(
        node: std::sync::Arc<crate::ast::SurfaceNode>,
        field: &'static str,
        ctx: Arc<crate::eval::EvalContext>,
        span: Span,
    ) -> Self {
        let (create_parent, create_time_us) = Self::profiling_data(&ctx);
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(UnevaluatedState::AstNodeField { node, field, ctx })),
                result: tokio::sync::OnceCell::new(),
            },
            span,
            origin: None,
            create_parent,
            create_time_us,
        }
    }

    pub fn new_pending_builtin(
        def: BuiltinDef,
        args: Vec<ThunkId>,
        named: Option<IndexMap<String, ThunkId>>,
        span: Span,
        origin: Option<Arc<str>>,
        caller_env_id: u32,
        ctx: Arc<crate::eval::EvalContext>,
    ) -> Self {
        let (create_parent, create_time_us) = Self::profiling_data(&ctx);
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(UnevaluatedState::Builtin {
                    def,
                    args,
                    named,
                    call_span: span.clone(),
                    caller_env_id,
                    ctx,
                })),
                result: tokio::sync::OnceCell::new(),
            },
            span,
            origin,
            create_parent,
            create_time_us,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_pending_call(
        func: ThunkId,
        args: Vec<ThunkId>,
        named: IndexMap<String, ThunkId>,
        call_span: Span,
        caller_env_id: u32,
        span: Span,
        origin: Option<Arc<str>>,
        ctx: Arc<crate::eval::EvalContext>,
        original_call: Arc<Spanned<CoreExpr>>,
    ) -> Self {
        let named_opt = if named.is_empty() {
            None
        } else {
            Some(Box::new(named))
        };
        let (create_parent, create_time_us) = Self::profiling_data(&ctx);
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(UnevaluatedState::Call {
                    func,
                    args,
                    named: named_opt,
                    call_span,
                    caller_env_id,
                    ctx,
                    original_call,
                })),
                result: tokio::sync::OnceCell::new(),
            },
            span,
            origin,
            create_parent,
            create_time_us,
        }
    }

    pub fn new_guarded(
        inner: ThunkId,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
    ) -> Self {
        Self::new_guarded_with_blame(inner, expected, field_path, guard_span, None)
    }

    pub fn new_guarded_with_blame(
        inner: ThunkId,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
        blame_label: Option<crate::error::BlameLabel>,
    ) -> Self {
        Self::new_guarded_full(inner, expected, field_path, guard_span, blame_label, None)
    }

    pub fn new_guarded_full(
        inner: ThunkId,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
        blame_label: Option<crate::error::BlameLabel>,
        default: Option<GuardDefault>,
    ) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(UnevaluatedState::Guarded {
                    inner,
                    expected,
                    field_path,
                    guard_span: guard_span.clone(),
                    blame_label,
                    default,
                })),
                result: tokio::sync::OnceCell::new(),
            },
            span: guard_span,
            origin: Some(Arc::from("type guard")),
            create_parent: None,
            create_time_us: 0,
        }
    }

    /// Set the origin label for this thunk (used in stack traces).
    pub fn with_origin(mut self, label: Arc<str>) -> Self {
        self.origin = Some(label);
        self
    }

    /// Return the source span where this thunk was created.
    pub fn definition_span(&self) -> Span {
        self.span.clone()
    }

    /// Restore unevaluated state after a non-cacheable error.
    pub(crate) fn restore_unevaluated(&self, state: UnevaluatedState) {
        *self.inner.unevaluated.lock().unwrap() = Some(state);
    }

    /// Create a new `Arc<Thunk>` that is identical to `self` but with `new_ctx` replacing
    /// the birth context in the unevaluated state.
    pub(crate) fn with_replaced_ctx(
        &self,
        new_ctx: Arc<crate::eval::EvalContext>,
    ) -> Option<Arc<Thunk>> {
        let guard = self.inner.unevaluated.lock().unwrap();
        let state = match guard.as_ref() {
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
            UnevaluatedState::Surface {
                node,
                res,
                types,
                env_id,
                ctx: _,
            } => UnevaluatedState::Surface {
                node,
                res,
                types,
                env_id,
                ctx: new_ctx,
            },
            UnevaluatedState::AstNodeField {
                node,
                field,
                ctx: _,
            } => UnevaluatedState::AstNodeField {
                node,
                field,
                ctx: new_ctx,
            },
            UnevaluatedState::Builtin {
                def,
                args,
                named,
                call_span,
                caller_env_id,
                ctx: _,
            } => UnevaluatedState::Builtin {
                def,
                args,
                named,
                call_span,
                caller_env_id,
                ctx: new_ctx,
            },
            UnevaluatedState::Call {
                func,
                args,
                named,
                call_span,
                caller_env_id,
                ctx: _,
                original_call,
            } => UnevaluatedState::Call {
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
        };
        Some(Arc::new(Thunk {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(new_state)),
                result: tokio::sync::OnceCell::new(),
            },
            span: self.span.clone(),
            origin: self.origin.clone(),
            create_parent: self.create_parent,
            create_time_us: self.create_time_us,
        }))
    }

    pub fn try_get_materialized(&self) -> Option<Value> {
        self.inner
            .result
            .get()
            .and_then(|r| r.as_ref().ok().cloned())
    }

    /// Check if the thunk is materialized without cloning the value.
    pub fn is_materialized(&self) -> bool {
        self.inner.result.get().is_some_and(|r| r.is_ok())
    }

    /// Set the thunk to materialized state with the given value.
    pub fn set_materialized(&self, value: Value) {
        *self.inner.unevaluated.lock().unwrap() = None;
        let _ = self.inner.result.set(Ok(value));
    }

    /// Atomically take the CoreExpr state (if present), transitioning to InProgress.
    pub fn take_core_expr(&self) -> Option<CoreExprParts> {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        match guard.take() {
            Some(UnevaluatedState::CoreExpr { expr, env_id, ctx }) => Some((expr, env_id, ctx)),
            other => {
                *guard = other;
                None
            }
        }
    }

    pub fn take_pending_builtin(&self) -> Option<PendingBuiltinParts> {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        match guard.take() {
            Some(UnevaluatedState::Builtin {
                def,
                args,
                named,
                call_span,
                caller_env_id,
                ctx,
            }) => Some((def, args, named, call_span, caller_env_id, ctx)),
            other => {
                *guard = other;
                None
            }
        }
    }

    pub fn take_pending_call(&self) -> Option<PendingCallParts> {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        match guard.take() {
            Some(UnevaluatedState::Call {
                func,
                args,
                named,
                call_span,
                caller_env_id,
                ctx,
                original_call,
            }) => {
                let named = named.map(|b| *b);
                Some((
                    func,
                    args,
                    named,
                    call_span,
                    caller_env_id,
                    ctx,
                    original_call,
                ))
            }
            other => {
                *guard = other;
                None
            }
        }
    }

    /// Extract Guarded state components and transition thunk to InProgress.
    pub fn take_guarded(&self) -> Option<GuardedParts> {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        match guard.take() {
            Some(UnevaluatedState::Guarded {
                inner,
                expected,
                field_path,
                guard_span,
                blame_label,
                default,
            }) => Some((
                inner,
                expected,
                field_path,
                guard_span,
                blame_label,
                default,
            )),
            other => {
                *guard = other;
                None
            }
        }
    }

    /// Atomically take the Surface state (if present), transitioning to InProgress.
    pub fn take_surface(&self) -> Option<SurfaceParts> {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        match guard.take() {
            Some(UnevaluatedState::Surface {
                node,
                res,
                types,
                env_id,
                ctx,
            }) => Some((node, res, types, env_id, ctx)),
            other => {
                *guard = other;
                None
            }
        }
    }

    /// Atomically take the AstNodeField state (if present), transitioning to InProgress.
    pub fn take_ast_node_field(
        &self,
    ) -> Option<(
        std::sync::Arc<crate::ast::SurfaceNode>,
        &'static str,
        Arc<crate::eval::EvalContext>,
    )> {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        match guard.take() {
            Some(UnevaluatedState::AstNodeField { node, field, ctx }) => Some((node, field, ctx)),
            other => {
                *guard = other;
                None
            }
        }
    }

    /// Return the cached error if the thunk is in Failed state.
    pub fn get_cached_error(&self) -> Option<Box<EvalError>> {
        self.inner.result.get().and_then(|r| {
            r.as_ref()
                .err()
                .map(|arc_err| Box::new((**arc_err).clone()))
        })
    }

    /// Return true if the thunk is currently in the InProgress (blackhole) state.
    pub fn is_in_progress(&self) -> bool {
        if self.inner.result.get().is_some() {
            return false;
        }
        self.inner.unevaluated.lock().unwrap().is_none()
    }

    /// Cache a failed evaluation by transitioning to the Failed state.
    pub fn cache_failure_once(&self, err: &EvalError) {
        if let Some(result) = self.inner.result.get() {
            if result.is_err() {
                return;
            }
        }
        *self.inner.unevaluated.lock().unwrap() = None;
        let _ = self.inner.result.set(Err(Arc::new(err.clone())));
    }

    // ========================================================================
    // Non-destructive introspection methods
    // ========================================================================

    /// Peek at the builtin def if this thunk is in Unevaluated Builtin state.
    pub fn peek_builtin_def(&self) -> Option<BuiltinDef> {
        let guard = self.inner.unevaluated.lock().unwrap();
        match &*guard {
            Some(UnevaluatedState::Builtin { def, .. }) => Some(*def),
            _ => None,
        }
    }

    /// Check if this thunk is in Guarded state.
    pub fn is_guarded(&self) -> bool {
        let guard = self.inner.unevaluated.lock().unwrap();
        matches!(&*guard, Some(UnevaluatedState::Guarded { .. }))
    }

    /// Check if this thunk is in PendingCall state.
    pub fn is_pending_call(&self) -> bool {
        let guard = self.inner.unevaluated.lock().unwrap();
        matches!(&*guard, Some(UnevaluatedState::Call { .. }))
    }

    /// Peek at the SurfaceNode if this thunk is in Surface state.
    pub fn peek_surface_node(&self) -> Option<std::sync::Arc<crate::ast::SurfaceNode>> {
        let guard = self.inner.unevaluated.lock().unwrap();
        match &*guard {
            Some(UnevaluatedState::Surface { node, .. }) => Some(std::sync::Arc::clone(node)),
            _ => None,
        }
    }

    /// Peek at the AstNodeField if this thunk is in AstNodeField state.
    pub fn peek_ast_node_field(
        &self,
    ) -> Option<(std::sync::Arc<crate::ast::SurfaceNode>, &'static str)> {
        let guard = self.inner.unevaluated.lock().unwrap();
        match &*guard {
            Some(UnevaluatedState::AstNodeField { node, field, .. }) => {
                Some((std::sync::Arc::clone(node), *field))
            }
            _ => None,
        }
    }

    /// Clone the current unevaluated state without consuming it (non-destructive peek).
    ///
    /// Returns `None` if the thunk is InProgress (unevaluated=None, result=empty),
    /// Materialized, or Failed. Returns `Some(state.clone())` if unevaluated.
    ///
    /// Used by arena migration to inspect env_id / ThunkId fields in an unevaluated
    /// thunk without taking ownership of the state.
    pub fn peek_unevaluated_state(&self) -> Option<UnevaluatedState> {
        let guard = self.inner.unevaluated.lock().unwrap();
        guard.as_ref().cloned()
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
                } else if guard.is_some() {
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
        if let Some(ref label) = self.origin {
            s.field("origin", label);
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
