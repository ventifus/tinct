//! Runtime value types: `Value`, `Thunk` (lazy memoization), `Environment` (lexical scope chain).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, RwLock};

use indexmap::{Equivalent, IndexMap};

use crate::ast::{Expr, Param, Span, Spanned, SurfaceDocument, SurfaceNode, SurfaceProgram};
use crate::error::{EvalError, EvalResult};
use crate::types::Type;

// Re-export ThunkId for use in other modules
pub use crate::arena::ThunkId;

/// Runtime metadata for user-defined functions — stored on `Value::Function`.
/// Enables runtime reflection via `ast-of` builtin and LSP features (hover, go-to-def).
#[derive(Clone, Debug)]
pub struct FnAnnotation {
    /// Doc string extracted from function's annotation metadata dict.
    pub doc: Option<String>,
    /// Source file path where the function was defined (if available).
    pub source_file: Option<String>,
}

/// Arguments passed to built-in functions.
///
/// Owns its arguments to allow capture in `async move` blocks that must be `'static`.
/// Previously used `&[Arc<Thunk>]` (a borrow), which caused lifetime errors when moved
/// into `Box<dyn Future>` (which has an implicit `'static` bound). Using owned `Vec`
/// avoids allocating lifetimes in the async state machine.
pub struct BuiltinArgs {
    pub args: Vec<Arc<Thunk>>,
    pub named: Option<IndexMap<String, Arc<Thunk>>>,
    pub call_span: Span,
    pub ctx: Arc<crate::eval::EvalContext>,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Int(i64),
    String(String),
}

impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Key::Int(a), Key::Int(b)) => a.partial_cmp(b),
            (Key::String(a), Key::String(b)) => a.partial_cmp(b),
            _ => None, // mixed types are incomparable
        }
    }
}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Key::Int(n) => n.hash(state),
            Key::String(s) => s.hash(state),
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Int(n) => write!(f, "{n}"),
            Key::String(s) => write!(f, "{s}"),
        }
    }
}

/// A wrapper type for `&str` that hashes the same way as `Key::String`.
/// This enables zero-allocation lookups in `IndexMap<Key, V>`.
#[derive(Debug)]
pub(crate) struct StrKey<'a>(pub &'a str);

impl Hash for StrKey<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Hash the String discriminant from Key enum
        std::mem::discriminant(&Key::String(String::new())).hash(state);
        // Then hash the string content
        self.0.hash(state);
    }
}

impl Equivalent<Key> for StrKey<'_> {
    fn equivalent(&self, key: &Key) -> bool {
        match key {
            Key::String(s) => self.0 == s.as_str(),
            Key::Int(_) => false,
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
        }
    }

    /// Parse a single letter mode (r/w/a/s/l) and return the corresponding permissions.
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
            }),
            'w' => Some(Self {
                readable: false,
                statable: false,
                listable: false,
                writable: true,
                appendable: true,
                deletable: true,
                renameable: true,
            }),
            'a' => Some(Self {
                readable: false,
                statable: false,
                listable: false,
                writable: false,
                appendable: true,
                deletable: false,
                renameable: false,
            }),
            's' => Some(Self {
                readable: false,
                statable: true,
                listable: false,
                writable: false,
                appendable: false,
                deletable: false,
                renameable: false,
            }),
            'l' => Some(Self {
                readable: false,
                statable: true,
                listable: true,
                writable: false,
                appendable: false,
                deletable: false,
                renameable: false,
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
        }
    }
}

/// A materialized runtime value.
#[derive(Clone)]
pub enum Value {
    /// 64-bit signed integer
    Int(i64),
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
    Dict(IndexMap<Key, ThunkId>),
    /// User-defined function (closure capturing its defining environment)
    Function {
        params: Rc<Vec<Param>>,
        body: Rc<Spanned<Expr>>,
        env: Arc<RwLock<Environment>>,
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
    /// `caps`: capability names → associated data (Null for boolean caps, Dict for protocol caps).
    /// `inner`: the underlying I/O reader (BufRead trait object).
    /// `write_inner`: optional write half for bidirectional connections (e.g. TCP sockets).
    /// `seek_inner`: optional seek interface for files (None for streams).
    /// `raw_tcp`: shared slot for extracting raw TcpStream (populated by `connect cap Tcp`, consumed by `tls-layer`).
    ///            `Rc<RefCell<Option<...>>>` preserves `Value: Clone` — all clones share the slot.
    ///            `take()` in `tls-layer` invalidates all aliases.
    /// `creation_span`: span where this Handle was created (for dual-span error messages).
    Handle {
        caps: HashMap<String, Value>,
        inner: Rc<std::cell::RefCell<Box<dyn std::io::BufRead>>>,
        write_inner: Option<Rc<std::cell::RefCell<Box<dyn std::io::Write>>>>,
        seek_inner: Option<Rc<std::cell::RefCell<Box<dyn std::io::Seek>>>>,
        raw_tcp: Option<Rc<RefCell<Option<std::net::TcpStream>>>>,
        creation_span: Span,
    },
    /// Write-only file/stream handle with capability metadata.
    /// `caps`: capability names → associated data (Null for boolean caps, Dict for protocol caps).
    /// `inner`: the underlying I/O writer (Write trait object).
    WriteHandle {
        caps: HashMap<String, Value>,
        inner: Rc<std::cell::RefCell<Box<dyn std::io::Write>>>,
    },
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
    /// Created via `decimal` builtin. No lossy cross-type with Float.
    Decimal(rust_decimal::Decimal),
    /// Arbitrary-precision integer (num_bigint::BigInt).
    /// Created via `big-int` builtin or arithmetic overflow promotion.
    BigInt(num_bigint::BigInt),
    /// Byte sequence (opaque binary data).
    /// Stored as a shared slice with byte offsets, enabling zero-copy subslicing.
    /// Used for binary I/O, cryptographic operations, and encoding conversions.
    Bytes {
        source: Rc<[u8]>,
        start: usize,
        end: usize,
    },
    /// URI — a uniform resource identifier with scheme and URI string.
    /// Used for capability-tagged URLs (e.g., http://, file://, mailto:).
    Uri { scheme: String, uri: String },
    /// UTC timestamp (nanoseconds since Unix epoch).
    /// Range: approximately 1678–2262 CE (±292 years from epoch).
    /// RFC 5280 certificate sentinel dates (9999-12-31) are clamped to i64::MAX.
    Timestamp(i64),
    /// Signed duration (nanoseconds).
    /// Not calendar-aware — no months/years, only seconds/minutes/hours/days.
    Duration(i64),
    /// Clock capability for reading current time (object capability model).
    ClockCap(Rc<ClockCapInner>),
    /// Timezone (parsed IANA TZ rules from zoneinfo file).
    /// Opaque — not serializable, consumed by timezone conversion builtins.
    Timezone(Rc<jiff::tz::TimeZone>),
    /// QUIC session — multiplexed connection over UDP (RFC 9000).
    /// Wraps a `quinn::Connection`. Created by `quic-session`, consumed by
    /// `quic-open-stream` and `quic-open-datagram`.
    QuicSession(Rc<quinn::Connection>),
    /// HTTP/2 session — multiplexed HTTP connection (RFC 9113).
    /// Wraps a `reqwest::blocking::Client` configured to prefer HTTP/2 via ALPN.
    /// Created by `http2-session`, consumed by `http-request`.
    /// `base_url` is the `scheme://host:port` origin used to resolve relative paths.
    Http2Session {
        client: Rc<reqwest::blocking::Client>,
        base_url: String,
    },
    /// HTTP/3 session — HTTP over QUIC (RFC 9114).
    /// Wraps an `Http3SessionState` containing the `h3::client::SendRequest` and the
    /// background driver `JoinHandle`. Created by `http3-session`, consumed by `http-request`.
    Http3Session(Rc<RefCell<Http3SessionState>>),
    /// QUIC datagram handle — unreliable message delivery over QUIC (RFC 9221).
    /// Wraps a `quinn::Connection` for datagram send/recv operations.
    /// Created by `quic-open-datagram`, consumed by `send-datagram` and `recv-datagram`.
    QuicDatagramHandle(Rc<quinn::Connection>),
    /// Message-oriented datagram socket (UDP or Unix datagram).
    /// Uses `send`/`recv` semantics (message boundaries preserved), not stream I/O.
    /// Created by `connect cap Udp host port` or `connect cap UnixDatagram path`.
    /// Consumed by `send-datagram` and `recv-datagram`.
    DatagramHandle {
        socket: DatagramSocket,
        creation_span: Span,
    },
    // DELETED: Value::RustRegistry (include-decomp-redelete sprint)
    // The %rust virtual module is now a plain Value::Dict injected into bootstrap_env.
    // See doc/whatif/include-decomposition.md.

    // =========================================================================
    // runtime-v2 native AST value types (Sprint 1, Part F)
    // =========================================================================
    //
    // These variants replace the old Dict-schema representation of AST nodes.
    // `load` returns Value::Program; `expand` takes and returns Value::Program;
    // `eval` takes [Seq Expression]; `ast-of` returns Value::Expression.
    //
    // `dict?` returns false for all three — they are nominal types, not plain Dicts.
    // Match dispatch works via `surface_expr_tag()` / `surface_doc_tag()` /
    // `surface_program_tag()` from `src/surface_fields.rs`.
    /// A complete tinct program — the type returned by `load` and `expand`.
    /// Wraps an Arc<SurfaceProgram> for Send+Sync compatibility (future async runtime).
    /// Also carries resolution and type annotation tables computed during load/expand.
    Program {
        program: Arc<SurfaceProgram>,
        resolutions: Arc<crate::ast::ResolutionTable>,
        types: Arc<crate::ast::TypeAnnotationTable>,
    },

    /// A single document within a program — accessible via `program.documents`.
    /// Contains expressions and declarations.
    Document(Arc<SurfaceDocument>),

    /// A single AST expression node — the type returned by `ast-of` and `[quote ...]`.
    /// Tinct code pattern-matches on this via the `Expression` type variants.
    Expression(Arc<SurfaceNode>),

    // =========================================================================
    // runtime-v2 async primitives (Sprint 2, Part B — real implementations)
    // =========================================================================
    //
    /// Async task handle — returned by `task` builtin, consumed by `await`.
    /// Uses tokio::task::spawn_local for !Send futures within a LocalSet.
    Task(Arc<tokio::sync::Mutex<TaskState>>),

    /// Channel for inter-task communication — created by `channel` builtin.
    /// Uses tokio::sync::mpsc for async send/recv operations.
    Channel(Arc<ChannelInner>),

    /// Cancellation context — created by `context` builtin, consumed by `with-cancel`.
    /// Skeleton added in Part F; full implementation deferred.
    Context,
}

/// State of an async task spawned via `task` builtin.
/// Tracks the JoinHandle while pending, caches the result once completed.
pub enum TaskState {
    /// Task is running — holds the JoinHandle.
    /// When awaited, polls the handle and transitions to Done.
    Pending(tokio::task::JoinHandle<EvalResult<Value>>),
    /// Task has completed — result is cached for subsequent awaits.
    /// Clone the Value when returning; keeps the cache intact.
    Done(EvalResult<Value>),
}

/// Inner state for a channel created via `channel` builtin.
/// Uses tokio::sync::mpsc for async send/recv operations.
pub struct ChannelInner {
    /// Sender half — cloned for each send operation.
    pub sender: tokio::sync::mpsc::Sender<Value>,
    /// Receiver half — wrapped in Mutex for exclusive access.
    /// Only one task can recv at a time.
    pub receiver: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<Value>>,
    /// Channel capacity (for debugging/introspection).
    pub capacity: i64,
}

/// State for an HTTP/3 session: the request sender and the background driver task.
///
/// The h3 protocol requires a connection-level "driver" future to be polled
/// concurrently with request streams — it processes incoming QUIC frames (SETTINGS,
/// GOAWAY, server push, etc.). We spawn it as a local task via `async_rt::spawn_local`
/// so it is polled every time `async_rt::block_on` drives the runtime.
///
/// Dropping the `_driver` `JoinHandle` would detach the task; keeping it here ensures
/// the driver is aborted when the session is dropped (all `Rc` clones released).
pub struct Http3SessionState {
    /// The request sender half — used by `http-request` to issue HTTP/3 requests.
    pub send_request: h3::client::SendRequest<h3_quinn::OpenStreams, bytes::Bytes>,
    /// Background driver task handle. Dropped (and task aborted) when the session is dropped.
    pub _driver: tokio::task::JoinHandle<()>,
}

/// Socket variant carried inside `Value::DatagramHandle`.
///
/// Both arms expose the same `send`/`recv` API after connection, so dispatch
/// is done once at construction time and builtins operate uniformly.
///
/// `Clone` is derived — `Arc::clone` is a shallow reference-count increment, not a socket copy.
#[derive(Clone, Debug)]
pub enum DatagramSocket {
    /// UDP socket, connected to a remote address via `UdpSocket::connect`.
    Udp(Rc<RefCell<std::net::UdpSocket>>),
    /// Unix-domain datagram socket (Linux/macOS), connected to a remote path.
    #[cfg(unix)]
    UnixDgram(Rc<RefCell<std::os::unix::net::UnixDatagram>>),
}

/// Helper function to construct a `Value::String` from a string slice.
/// Creates a new `Rc<str>` and uses the full range (0..len).
pub fn string_val(s: &str) -> Value {
    Value::String {
        source: Rc::from(s),
        start: 0,
        end: s.len(),
    }
}

/// Helper function to construct a `Value::Bytes` from a byte slice.
/// Creates a new `Rc<[u8]>` and uses the full range (0..len).
pub fn bytes_val(data: &[u8]) -> Value {
    Value::Bytes {
        source: Rc::from(data),
        start: 0,
        end: data.len(),
    }
}

impl Value {
    /// Returns the human-readable type name for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::String { .. } => "String",
            Value::Bool(_) => "Bool",
            Value::Dict(_) => "Dict",
            Value::Function { .. } => "Function",
            Value::Builtin(_) => "Builtin",
            Value::Seq { .. } => "Seq",
            Value::Proxy { .. } => "Proxy",
            Value::Overlay(..) => "Dict",
            Value::DirCap { .. } => "DirCap",
            Value::NetCap(_) => "NetCap",
            Value::Handle { .. } => "Handle",
            Value::WriteHandle { .. } => "WriteHandle",
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
            Value::Context => "Context",
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
            Value::Float(n) => f.debug_tuple("Float").field(n).finish(),
            Value::String { source, start, end } => {
                let s = &source[*start..*end];
                f.debug_tuple("String").field(&s).finish()
            }
            Value::Bool(b) => f.debug_tuple("Bool").field(b).finish(),
            Value::Dict(map) => {
                let keys: Vec<&Key> = map.keys().collect();
                f.debug_tuple("Dict").field(&keys).finish()
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
            Value::Context => write!(f, "Context"),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{n}"),
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
            Value::Function { params, .. } => {
                write!(f, "[fn [")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{}", p.name)?;
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
            Value::Timestamp(nanos) => {
                // Convert nanoseconds to jiff::Timestamp for display
                match jiff::Timestamp::from_nanosecond(*nanos as i128) {
                    Ok(ts) => write!(f, "{ts}"),
                    Err(_) => write!(f, "<invalid timestamp>"),
                }
            }
            Value::Duration(nanos) => {
                // Display as signed nanoseconds
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
            Value::Context => write!(f, "<context>"),
        }
    }
}

/// Compares primitives (Int, Float, String, Bool) by value; cross-variant
/// comparison always returns false (e.g. `Int(1) != Float(1.0)`). Float uses
/// IEEE 754 semantics (NaN != NaN). Dict, Function, Builtin, Seq, and Proxy are
/// intentionally non-comparable and always return false, even to themselves.
///
/// # Hash Consistency Invariant
///
/// **REQUIREMENT:** For Dict key equality, `hash(a) == hash(b)` whenever
/// `Value::PartialEq` returns `a == b`.
///
/// **CURRENT STATUS:** This invariant is SATISFIED. Int and Float use separate
/// hash paths in `Key::hash()` (via discriminant-based hashing), so `Int(1)` and
/// `Float(1.0)` produce different hashes even though they are numerically equal.
/// Cross-variant comparisons return `false` in `Value::PartialEq`, so distinct
/// hash values are required.
///
/// **CONSEQUENCE:** Dict keys `[1: x]` (Int key) and `[1.0: y]` (Float key) are
/// treated as DISTINCT keys, not merged. This is intentional: the type system
/// treats `Int` and `Float` as separate types (subsumed by `Number` via subtyping,
/// but not equal). Future Dict key deduplication or Set types must preserve this
/// separation.
///
/// **CROSS-REFERENCE:** `Key::hash()` implementation (lines 47-54) enforces
/// discriminant-based hashing to maintain this invariant. See also: `Key::PartialEq`
/// (derived via `#[derive(PartialEq)]` on line 31) and `$=` builtin semantics
/// (src/builtins_math.rs) which allow cross-type Int/Float comparison (separate
/// from Value equality used for Dict keys).
///
/// Structural equality for thunk memoization. Note: differs from `$=` (which promotes
/// Int→Float for cross-type comparison). `Value::PartialEq` uses structural equality;
/// `$=` uses arithmetic promotion.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
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
            ) => &src_a[*start_a..*end_a] == &src_b[*start_b..*end_b],
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
            ) => &src_a[*start_a..*end_a] == &src_b[*start_b..*end_b],
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
            // Timezone is not comparable — opaque data
            // Dict, Function, Builtin, Seq, Proxy, Overlay, Handle, and WriteHandle are not structurally compared.
            // Overlay would require materializing both sides, breaking laziness.
            // Handle and WriteHandle cannot be meaningfully compared (contain RefCell and trait objects).
            _ => false,
        }
    }
}

// Size assertion: ensure Value::Dict (IndexMap) remains the dominant variant.
// Value enum size is dominated by Dict(IndexMap) + Handle. BuiltinDef is Copy (40 bytes:
// 8-byte fn ptr + 2 fat ptrs for &str and &[Strictness]). IndexMap size varies
// by version; indexmap 2.x uses ~72 bytes on 64-bit platforms.
// Handle added raw_tcp (16 bytes) + creation_span (48 bytes) in connect-v2 refactor.
const _: () = {
    const EXPECTED_MAX: usize = 144; // Increased from 80 for Handle refactor
    const ACTUAL: usize = std::mem::size_of::<Value>();
    assert!(
        ACTUAL <= EXPECTED_MAX,
        "Value size increased beyond expected maximum"
    );
};

// ============================================================================
// Runtime v2 — Sprint 2B: ThunkInner + UnevaluatedState
// ============================================================================

/// Pre-evaluation state variants for the ThunkInner structure.
/// Stores the data needed to evaluate a thunk when it's first accessed.
#[derive(Debug)]
pub enum UnevaluatedState {
    /// AST expression from the old runtime (CoreExpr will replace this in full runtime-v2).
    Expr {
        expr: Rc<Spanned<Expr>>,
        env: Arc<RwLock<Environment>>,
        #[allow(dead_code)]
        env_id: Option<crate::arena::EnvId>,
        ctx: Arc<crate::eval::EvalContext>,
    },
    /// Pre-lowering Surface thunk — created by the `eval` builtin.
    Surface {
        node: Arc<SurfaceNode>,
        res: Arc<crate::ast::ResolutionTable>,
        types: Arc<crate::ast::TypeAnnotationTable>,
        env: Arc<RwLock<Environment>>,
        ctx: Arc<crate::eval::EvalContext>,
    },
    /// Lazy AST node field access via `surface_node_get_field`.
    AstNodeField {
        node: Arc<SurfaceNode>,
        field: &'static str,
        ctx: Arc<crate::eval::EvalContext>,
    },
    /// Deferred builtin call (was PendingBuiltin).
    Builtin {
        def: BuiltinDef,
        args: Box<Vec<Arc<Thunk>>>,
        named: Option<IndexMap<String, Arc<Thunk>>>,
        call_span: Span,
        ctx: Arc<crate::eval::EvalContext>,
    },
    /// Deferred function call (was PendingCall).
    Call {
        func: Arc<Thunk>,
        args: Box<Vec<Arc<Thunk>>>,
        named: Option<Box<IndexMap<String, Arc<Thunk>>>>,
        call_span: Span,
        caller_env: Arc<RwLock<Environment>>,
        ctx: Arc<crate::eval::EvalContext>,
    },
    /// Type guard wrapping an inner thunk (was Guarded).
    Guarded {
        inner: Arc<Thunk>,
        expected: Type,
        field_path: Box<Vec<String>>,
        guard_span: Span,
        blame_label: Option<crate::error::BlameLabel>,
        default: Option<(Rc<Spanned<Expr>>, Arc<RwLock<Environment>>)>,
    },
}

/// New thunk structure for async evaluation (Sprint 2B).
/// Replaces Mutex<ThunkState> with a two-field pair:
/// - unevaluated: taken (set to None) when evaluation starts
/// - result: set exactly once when evaluation completes
///
/// This is ADDITIVE — Thunk still uses Mutex<ThunkState> during the transition.
#[derive(Debug)]
pub struct ThunkInner {
    /// Pre-evaluation state. Set to Some initially, taken (set to None) when evaluation starts.
    /// Taking this field atomically transitions the thunk to "InProgress" state.
    pub unevaluated: Mutex<Option<UnevaluatedState>>,

    /// Post-evaluation result. Set exactly once when evaluation completes (success or failure).
    /// Cycle detection: if unevaluated is None and result is not yet set → circular dependency.
    pub result: tokio::sync::OnceCell<Result<Value, Arc<EvalError>>>,
}

/// Lazy evaluation cell: wraps an unevaluated expression, a pending builtin call,
/// or a materialized value with memoization (evaluate-at-most-once semantics).
pub struct Thunk {
    inner: ThunkInner,
    pub(crate) span: Span,
    /// Label describing this thunk's origin (e.g. "call $f").
    /// `None` for anonymous thunks (the common case); eliminates per-thunk String allocation.
    /// Used for stack trace construction when materialization fails.
    pub(crate) origin: Option<Arc<str>>,
}

impl Thunk {
    /// Create a placeholder thunk for letrec pre-allocation. Must be filled via
    /// `set_state()` before use. Panics at materialization if still in Placeholder state.
    pub fn new_placeholder(span: Span) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(None), // Placeholder: no unevaluated state, no result
                result: tokio::sync::OnceCell::new(),
            },
            span,
            origin: None,
        }
    }

    pub fn new_unevaluated(
        expr: Rc<Spanned<Expr>>,
        env: Arc<RwLock<Environment>>,
        ctx: Arc<crate::eval::EvalContext>,
        span: Span,
    ) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(UnevaluatedState::Expr {
                    expr,
                    env,
                    env_id: None,
                    ctx,
                })),
                result: tokio::sync::OnceCell::new(),
            },
            span,
            origin: None,
        }
    }

    /// Create an unevaluated thunk with a flat environment ID for O(1) variable lookup.
    ///
    /// The `env_id` parameter enables the O(1) variable lookup path when the resolver
    /// has populated VarRef coordinates. The Arc<RwLock<Environment>> chain remains
    /// as a fallback for stdlib bindings and computed keys.
    pub fn new_unevaluated_with_env_id(
        expr: Rc<Spanned<Expr>>,
        env: Arc<RwLock<Environment>>,
        env_id: crate::arena::EnvId,
        ctx: Arc<crate::eval::EvalContext>,
        span: Span,
    ) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(UnevaluatedState::Expr {
                    expr,
                    env,
                    env_id: Some(env_id),
                    ctx,
                })),
                result: tokio::sync::OnceCell::new(),
            },
            span,
            origin: None,
        }
    }

    pub fn new_materialized(value: Value, span: Span) -> Self {
        let inner = ThunkInner {
            unevaluated: Mutex::new(None),
            result: tokio::sync::OnceCell::new(),
        };
        // Set the result directly (fast-path for literals)
        let _ = inner.result.set(Ok(value));
        Self {
            inner,
            span,
            origin: None,
        }
    }

    /// Create a Surface thunk — wraps a SurfaceNode for lazy evaluation.
    ///
    /// On first force, the Surface thunk is evaluated via the evaluator which converts
    /// it through the bridge to the old Expr format and evaluates it.
    /// (Full CoreExpr lowering path is Sprint 2.)
    pub fn new_surface(
        node: std::sync::Arc<crate::ast::SurfaceNode>,
        res: std::sync::Arc<crate::ast::ResolutionTable>,
        types: std::sync::Arc<crate::ast::TypeAnnotationTable>,
        env: Arc<RwLock<Environment>>,
        ctx: Arc<crate::eval::EvalContext>,
        span: Span,
    ) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(UnevaluatedState::Surface {
                    node,
                    res,
                    types,
                    env,
                    ctx,
                })),
                result: tokio::sync::OnceCell::new(),
            },
            span,
            origin: None,
        }
    }

    /// Create a lazy AstNodeField thunk — evaluates a single named field from a SurfaceNode.
    ///
    /// `field` must be a `'static str` (a literal field name like "name", "args", "span").
    /// `ctx` is needed to allocate ThunkIds for sequence-typed fields.
    pub fn new_ast_node_field(
        node: std::sync::Arc<crate::ast::SurfaceNode>,
        field: &'static str,
        ctx: Arc<crate::eval::EvalContext>,
        span: Span,
    ) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(UnevaluatedState::AstNodeField { node, field, ctx })),
                result: tokio::sync::OnceCell::new(),
            },
            span,
            origin: None,
        }
    }

    /// `named`: pass `None` when there are no named args (the common case for internal
    /// thunks); pass `Some(map)` only when named args are actually present.
    pub fn new_pending_builtin(
        def: BuiltinDef,
        args: Vec<Arc<Thunk>>,
        named: Option<IndexMap<String, Arc<Thunk>>>,
        span: Span,
        origin: Option<Arc<str>>,
        ctx: Arc<crate::eval::EvalContext>,
    ) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(UnevaluatedState::Builtin {
                    def,
                    args: Box::new(args),
                    named,
                    call_span: span,
                    ctx,
                })),
                result: tokio::sync::OnceCell::new(),
            },
            span,
            origin,
        }
    }

    pub fn new_pending_call(
        func: Arc<Thunk>,
        args: Vec<Arc<Thunk>>,
        named: IndexMap<String, Arc<Thunk>>,
        call_span: Span,
        caller_env: Arc<RwLock<Environment>>,
        span: Span,
        origin: Option<Arc<str>>,
        ctx: Arc<crate::eval::EvalContext>,
    ) -> Self {
        let named_opt = if named.is_empty() {
            None
        } else {
            Some(Box::new(named))
        };
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(UnevaluatedState::Call {
                    func,
                    args: Box::new(args),
                    named: named_opt,
                    call_span,
                    caller_env,
                    ctx,
                })),
                result: tokio::sync::OnceCell::new(),
            },
            span,
            origin,
        }
    }

    pub fn new_guarded(
        inner: Arc<Thunk>,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
    ) -> Self {
        Self::new_guarded_with_blame(inner, expected, field_path, guard_span, None)
    }

    pub fn new_guarded_with_blame(
        inner: Arc<Thunk>,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
        blame_label: Option<crate::error::BlameLabel>,
    ) -> Self {
        Self::new_guarded_full(inner, expected, field_path, guard_span, blame_label, None)
    }

    pub fn new_guarded_full(
        inner: Arc<Thunk>,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
        blame_label: Option<crate::error::BlameLabel>,
        default: Option<(
            Rc<crate::ast::Spanned<crate::ast::Expr>>,
            Arc<RwLock<Environment>>,
        )>,
    ) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new(Some(UnevaluatedState::Guarded {
                    inner,
                    expected,
                    field_path: Box::new(field_path),
                    guard_span,
                    blame_label,
                    default,
                })),
                result: tokio::sync::OnceCell::new(),
            },
            span: guard_span,
            origin: Some(Arc::from("type guard")),
        }
    }

    /// Set the origin label for this thunk (used in stack traces).
    pub fn with_origin(mut self, label: Arc<str>) -> Self {
        self.origin = Some(label);
        self
    }

    /// Restore unevaluated state after a non-cacheable error.
    /// Used only for error recovery in eval.rs and eval_materialize.rs.
    pub(crate) fn restore_unevaluated(&self, state: UnevaluatedState) {
        *self.inner.unevaluated.lock().unwrap() = Some(state);
    }

    pub fn try_get_materialized(&self) -> Option<Value> {
        self.inner
            .result
            .get()
            .and_then(|r| r.as_ref().ok().cloned())
    }

    /// Check if the thunk is materialized without cloning the value.
    pub fn is_materialized(&self) -> bool {
        self.inner.result.get().map_or(false, |r| r.is_ok())
    }

    /// Set the thunk to materialized state with the given value.
    /// Clears the unevaluated slot and writes `Ok(value)` to the result OnceCell.
    pub fn set_materialized(&self, value: Value) {
        *self.inner.unevaluated.lock().unwrap() = None;
        let _ = self.inner.result.set(Ok(value));
    }

    /// Take ownership of unevaluated data, atomically setting state to InProgress.
    /// Returns None if the thunk is not in the Unevaluated state.
    #[allow(clippy::type_complexity)]
    pub fn take_unevaluated(
        &self,
    ) -> Option<(
        Rc<Spanned<Expr>>,
        Arc<RwLock<Environment>>,
        Arc<crate::eval::EvalContext>,
    )> {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        match guard.take() {
            Some(UnevaluatedState::Expr { expr, env, ctx, .. }) => {
                // State is now InProgress (unevaluated = None, result = empty)
                Some((expr, env, ctx))
            }
            other => {
                // Restore the state
                *guard = other;
                None
            }
        }
    }

    // Return type is a one-shot destructured tuple only used in materialize();
    // a type alias would add indirection without clarity.
    #[allow(clippy::type_complexity)]
    pub fn take_pending_builtin(
        &self,
    ) -> Option<(
        BuiltinDef,
        Vec<Arc<Thunk>>,
        Option<IndexMap<String, Arc<Thunk>>>,
        Span,
        Arc<crate::eval::EvalContext>,
    )> {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        match guard.take() {
            Some(UnevaluatedState::Builtin {
                def,
                args,
                named,
                call_span,
                ctx,
            }) => {
                // State is now InProgress
                Some((def, *args, named, call_span, ctx))
            }
            other => {
                // Restore the state
                *guard = other;
                None
            }
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn take_pending_call(
        &self,
    ) -> Option<(
        Arc<Thunk>,
        Vec<Arc<Thunk>>,
        Option<IndexMap<String, Arc<Thunk>>>,
        Span,
        Arc<RwLock<Environment>>,
        Arc<crate::eval::EvalContext>,
    )> {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        match guard.take() {
            Some(UnevaluatedState::Call {
                func,
                args,
                named,
                call_span,
                caller_env,
                ctx,
            }) => {
                // State is now InProgress
                // Convert Option<Box<IndexMap>> to Option<IndexMap>
                let named = named.map(|b| *b);
                Some((func, *args, named, call_span, caller_env, ctx))
            }
            other => {
                // Restore the state
                *guard = other;
                None
            }
        }
    }

    /// Extract Guarded state components and transition thunk to InProgress.
    ///
    /// This is NOT a simple accessor - it has side effects:
    /// - If the thunk is Guarded, it transitions to InProgress and returns the components.
    ///   The caller is responsible for transitioning to Materialized or Failed after processing.
    /// - If the thunk is NOT Guarded, the state is restored unchanged and None is returned.
    ///
    /// The InProgress transition prevents re-entrance during guard materialization.
    pub fn take_guarded(
        &self,
    ) -> Option<(
        Arc<Thunk>,
        Type,
        Vec<String>,
        Span,
        Option<crate::error::BlameLabel>,
        Option<(
            Rc<crate::ast::Spanned<crate::ast::Expr>>,
            Arc<RwLock<Environment>>,
        )>,
    )> {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        match guard.take() {
            Some(UnevaluatedState::Guarded {
                inner,
                expected,
                field_path,
                guard_span,
                blame_label,
                default,
            }) => {
                // State is now InProgress
                Some((
                    inner,
                    expected,
                    *field_path,
                    guard_span,
                    blame_label,
                    default,
                ))
            }
            other => {
                // Restore the state
                *guard = other;
                None
            }
        }
    }

    /// Atomically take the Surface state (if present), transitioning to InProgress.
    ///
    /// Returns `Some((node, res, types, env, ctx))` if the thunk was in Surface state.
    /// Returns `None` if the thunk was in any other state (state is restored).
    pub fn take_surface(
        &self,
    ) -> Option<(
        std::sync::Arc<crate::ast::SurfaceNode>,
        std::sync::Arc<crate::ast::ResolutionTable>,
        std::sync::Arc<crate::ast::TypeAnnotationTable>,
        Arc<RwLock<Environment>>,
        Arc<crate::eval::EvalContext>,
    )> {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        match guard.take() {
            Some(UnevaluatedState::Surface {
                node,
                res,
                types,
                env,
                ctx,
            }) => {
                // State is now InProgress
                Some((node, res, types, env, ctx))
            }
            other => {
                // Restore the state
                *guard = other;
                None
            }
        }
    }

    /// Atomically take the AstNodeField state (if present), transitioning to InProgress.
    ///
    /// Returns `Some((node, field))` if the thunk was in AstNodeField state.
    /// Returns `None` if the thunk was in any other state (state is restored).
    pub fn take_ast_node_field(
        &self,
    ) -> Option<(
        std::sync::Arc<crate::ast::SurfaceNode>,
        &'static str,
        Arc<crate::eval::EvalContext>,
    )> {
        let mut guard = self.inner.unevaluated.lock().unwrap();
        match guard.take() {
            Some(UnevaluatedState::AstNodeField { node, field, ctx }) => {
                // State is now InProgress
                Some((node, field, ctx))
            }
            other => {
                // Restore the state
                *guard = other;
                None
            }
        }
    }

    /// Return the cached error if the thunk is in Failed state, without holding a ThunkStateGuard.
    ///
    /// Returns `Some(err)` if the thunk has a cached error (Failed state).
    /// Returns `None` for all other states (Materialized, InProgress, or any deferred state).
    ///
    /// Prefer this over `thunk.state()` when only the error case needs to be checked,
    /// as it avoids the ThunkStateGuard aliasing hazard.
    pub fn get_cached_error(&self) -> Option<Box<EvalError>> {
        self.inner.result.get().and_then(|r| {
            r.as_ref()
                .err()
                .map(|arc_err| Box::new((**arc_err).clone()))
        })
    }

    /// Return true if the thunk is currently in the InProgress (blackhole) state.
    ///
    /// The InProgress state means `unevaluated` is `None` (taken atomically) and
    /// `result` has not yet been set. This is the cycle-detection sentinel.
    ///
    /// Note: `Placeholder` thunks (created via `new_placeholder`) are also represented
    /// as (unevaluated=None, result=empty) in `ThunkInner`, so they are indistinguishable
    /// from `InProgress` at the storage level. The cycle-detection path is correct for both:
    /// a Placeholder that gets forced is a letrec construction bug, and returning a
    /// circular-dependency error (rather than panicking) is acceptable.
    pub fn is_in_progress(&self) -> bool {
        // InProgress = unevaluated slot is empty AND result is not yet set.
        // We check result first (cheapest: no lock) then unevaluated.
        if self.inner.result.get().is_some() {
            return false; // Materialized or Failed
        }
        self.inner.unevaluated.lock().unwrap().is_none()
    }

    /// Cache a failed evaluation by transitioning to the Failed state.
    /// Used to memoize errors so failed thunks don't re-evaluate on subsequent access.
    ///
    /// Skips the clone and state write if the thunk is already in the Failed state
    /// (e.g., when a shared thunk is encountered a second time during error propagation).
    pub fn cache_failure(&self, err: &EvalError) {
        // Fast path: if already Failed, no work needed — avoid the clone.
        if let Some(result) = self.inner.result.get() {
            if result.is_err() {
                return;
            }
        }

        // Clear unevaluated state and set error result
        *self.inner.unevaluated.lock().unwrap() = None;
        let _ = self.inner.result.set(Err(Arc::new(err.clone())));
    }

    // ========================================================================
    // Non-destructive introspection methods (for builtin_ast_of, debugging)
    // ========================================================================

    /// Peek at the expression if this thunk is in Unevaluated Expr state.
    /// Does not force or transition the thunk.
    pub fn peek_expr(&self) -> Option<Rc<Spanned<Expr>>> {
        let guard = self.inner.unevaluated.lock().unwrap();
        match &*guard {
            Some(UnevaluatedState::Expr { expr, .. }) => Some(expr.clone()),
            _ => None,
        }
    }

    /// Peek at the builtin def if this thunk is in Unevaluated Builtin state.
    /// Does not force or transition the thunk.
    pub fn peek_builtin_def(&self) -> Option<BuiltinDef> {
        let guard = self.inner.unevaluated.lock().unwrap();
        match &*guard {
            Some(UnevaluatedState::Builtin { def, .. }) => Some(def.clone()),
            _ => None,
        }
    }

    /// Check if this thunk is in Guarded state.
    /// Does not force or transition the thunk.
    pub fn is_guarded(&self) -> bool {
        let guard = self.inner.unevaluated.lock().unwrap();
        matches!(&*guard, Some(UnevaluatedState::Guarded { .. }))
    }

    /// Check if this thunk is in PendingCall state.
    /// Does not force or transition the thunk.
    pub fn is_pending_call(&self) -> bool {
        let guard = self.inner.unevaluated.lock().unwrap();
        matches!(&*guard, Some(UnevaluatedState::Call { .. }))
    }

    /// Peek at the SurfaceNode if this thunk is in Surface state.
    /// Does not force or transition the thunk.
    pub fn peek_surface_node(&self) -> Option<std::sync::Arc<crate::ast::SurfaceNode>> {
        let guard = self.inner.unevaluated.lock().unwrap();
        match &*guard {
            Some(UnevaluatedState::Surface { node, .. }) => Some(std::sync::Arc::clone(node)),
            _ => None,
        }
    }

    /// Peek at the AstNodeField if this thunk is in AstNodeField state.
    /// Returns (node, field_name) tuple.
    /// Does not force or transition the thunk.
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
}

impl fmt::Debug for Thunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("Thunk");

        // Try to get the state without blocking
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
/// Uses `IndexMap` to preserve insertion order — a prerequisite for Phase 2's slot-based
/// lookup where `(level, slot)` indices reference entries by position. Phase 1 (resolver)
/// populates the caches; the evaluator still uses name-based lookup until FlatEnv exists.
///
/// # DAG invariant
///
/// The parent chain forms a directed acyclic graph (DAG), not a cyclic graph. This
/// invariant is guaranteed structurally by lexical scoping: each `Environment` is created
/// from a parent that already exists (the enclosing scope at the time of evaluation), so
/// no environment can transitively point back to itself. Specifically:
///
/// - `eval()` creates a child env by calling `Environment::with_parent(env)` on the
///   **current** scope before evaluating the body. The body is evaluated in the child;
///   the parent cannot reference the child.
/// - `letrec` dict bindings share a single pre-allocated environment, but all thunks in
///   that env point to the same env — a self-referential structure that is still acyclic
///   in the parent chain (no environment has itself as an ancestor).
/// - `$include` creates a fresh child env from the stdlib root; it cannot create cycles.
///
/// The absence of cycles means `Environment::get()` always terminates. It also means
/// environments form a tree rooted at the stdlib environment, enabling safe stack-free
/// traversal via iterative parent-pointer walking.
#[derive(Debug, Clone)]
pub struct Environment {
    /// Bindings map from name to thunk. Uses IndexMap to preserve insertion order for
    /// future slot-based O(1) lookup (Phase 2). Phase 1 resolution pass populates VarRef
    /// caches but evaluator still uses name-based lookup.
    pub(crate) bindings: IndexMap<String, Arc<Thunk>>,
    pub(crate) parent: Option<Arc<RwLock<Environment>>>,
}

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
    ///
    /// # Lock safety
    ///
    /// This method acquires a read lock on each ancestor `RwLock<Environment>` as
    /// it walks up the scope chain.  Callers **must not** hold a write lock
    /// on any ancestor environment while calling `get()`, or
    /// the program will deadlock.
    ///
    /// The scope chain must form a DAG -- circular parent links will cause an
    /// infinite loop.
    pub fn get(&self, name: &str) -> Option<Arc<Thunk>> {
        // Check current scope first
        if let Some(thunk) = self.bindings.get(name) {
            return Some(Arc::clone(thunk));
        }
        // Walk parent chain iteratively
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
    ///
    /// `level` is a De Bruijn index: 0 = current environment, 1 = parent, N = Nth ancestor.
    /// This matches the resolver's level assignment in `resolve.rs::Resolver::resolve`.
    ///
    /// For `level = 0`: looks up `slot` directly in the current environment's
    /// `bindings` IndexMap using `get_index` — no name hash, no string comparison.
    ///
    /// For `level > 0`: walks `level` steps up the parent chain, then does the
    /// slot lookup. Each step costs one `Arc::clone` + `RwLock::read`, so the
    /// total cost is O(level). This is still faster than name-based lookup for
    /// deep environments because we skip the string hash at each level.
    ///
    /// Returns `None` if the level or slot is out of bounds (indicates a resolver
    /// bug; `eval.rs` falls back to name-based lookup when this returns `None`).
    pub fn get_by_slot(&self, level: u32, slot: u32) -> Option<Arc<Thunk>> {
        if level == 0 {
            // Fast path: O(1) index into the current scope's bindings
            return self
                .bindings
                .get_index(slot as usize)
                .map(|(_, thunk)| Arc::clone(thunk));
        }
        // Walk `level` steps up the parent chain, then do slot lookup
        let mut steps_remaining = level;
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_rc) = current {
            steps_remaining -= 1;
            if steps_remaining == 0 {
                let env = env_rc.read().unwrap();
                return env
                    .bindings
                    .get_index(slot as usize)
                    .map(|(_, thunk)| Arc::clone(thunk));
            }
            let next = env_rc.read().unwrap().parent.as_ref().map(Arc::clone);
            current = next;
        }
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
    use crate::test_util::test_span;

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let env = Arc::new(RwLock::new(Environment::new()));
        crate::eval::EvalContext::new(base_dir, Arc::clone(&env), Arc::clone(&env), false)
    }

    #[test]
    fn test_key_hash_consistency() {
        use std::collections::hash_map::DefaultHasher;

        let k1 = Key::String("x".into());
        let k2 = Key::String("x".into());

        let hash = |k: &Key| {
            let mut h = DefaultHasher::new();
            k.hash(&mut h);
            h.finish()
        };

        assert_eq!(hash(&k1), hash(&k2));
    }

    #[test]
    fn test_key_display() {
        assert_eq!(format!("{}", Key::Int(42)), "42");
        assert_eq!(format!("{}", Key::String("hello".into())), "hello");
    }

    #[test]
    fn test_value_partial_eq_primitives() {
        assert_eq!(Value::Int(1), Value::Int(1));
        assert_ne!(Value::Int(1), Value::Int(2));
        assert_eq!(Value::Float(3.14), Value::Float(3.14));
        assert_eq!(string_val("a"), string_val("a"));
        assert_ne!(string_val("a"), string_val("b"));
        assert_eq!(Value::Bool(true), Value::Bool(true));
        assert_ne!(Value::Bool(true), Value::Bool(false));
    }

    #[test]
    fn test_value_partial_eq_cross_variant() {
        assert_ne!(Value::Int(1), Value::Float(1.0));
        assert_ne!(Value::Int(0), Value::Bool(false));
        assert_ne!(string_val("1"), Value::Int(1));
    }

    #[test]
    fn test_value_partial_eq_dict_always_false() {
        let d1 = Value::Dict(IndexMap::new());
        let d2 = Value::Dict(IndexMap::new());
        assert_ne!(d1, d2);
    }

    #[test]
    fn test_value_partial_eq_nan() {
        assert_ne!(Value::Float(f64::NAN), Value::Float(f64::NAN));
    }

    #[test]
    fn test_value_partial_eq_function_always_false() {
        // Function values are intentionally non-comparable
        let f = Value::Function {
            params: Rc::new(vec![]),
            body: Rc::new(Spanned::new(Expr::Int(0), test_span(1, 1, 1, 1))),
            env: Arc::new(RwLock::new(Environment::new())),
            annotation: None,
        };
        assert_ne!(f.clone(), f);
    }

    #[test]
    fn test_value_partial_eq_builtin_always_false() {
        fn dummy(ctx: BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Int(0),
                    ctx.call_span,
                )))
            })
        }
        let b = Value::Builtin(BuiltinDef {
            func: dummy,
            name: "test",
            pos_strictness: &[],
            force_count: 0,
        });
        assert_ne!(b.clone(), b);
    }

    #[test]
    fn test_builtin_def_partial_eq_by_name() {
        // BuiltinDef equality is name-based, not function-pointer-based.
        // Two BuiltinDefs with the same name must compare equal regardless of their
        // function pointers; two with different names must compare unequal.
        fn func_a(ctx: BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Int(1),
                    ctx.call_span,
                )))
            })
        }
        fn func_b(ctx: BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Int(2),
                    ctx.call_span,
                )))
            })
        }

        let same_name_a = BuiltinDef {
            func: func_a,
            name: "my-builtin",
            pos_strictness: &[],
            force_count: 0,
        };
        let same_name_b = BuiltinDef {
            func: func_b, // different function pointer, same name
            name: "my-builtin",
            pos_strictness: &[Strictness::Seq],
            force_count: 0,
        };
        let different_name = BuiltinDef {
            func: func_a,
            name: "other-builtin",
            pos_strictness: &[],
            force_count: 0,
        };

        assert_eq!(
            same_name_a, same_name_b,
            "BuiltinDefs with the same name must compare equal regardless of function pointer"
        );
        assert_ne!(
            same_name_a, different_name,
            "BuiltinDefs with different names must compare unequal"
        );
    }

    #[test]
    fn test_seq_type_name() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 1);
        let seq = Value::Seq {
            head: ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(42), span))),
            tail: ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span,
            ))),
        };
        assert_eq!(seq.type_name(), "Seq");
    }

    #[test]
    fn test_seq_debug() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 1);
        let seq = Value::Seq {
            head: ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(1), span))),
            tail: ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span,
            ))),
        };
        let debug_str = format!("{:?}", seq);
        assert_eq!(debug_str, "Seq(...)");
    }

    #[test]
    fn test_seq_display() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 1);
        let seq = Value::Seq {
            head: ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(1), span))),
            tail: ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span,
            ))),
        };
        let display_str = format!("{}", seq);
        assert_eq!(display_str, "Seq(...)");
    }

    #[test]
    fn test_seq_not_equal_to_itself() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 1);
        let seq = Value::Seq {
            head: ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(42), span))),
            tail: ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span,
            ))),
        };
        assert_ne!(seq.clone(), seq);
    }

    #[test]
    fn test_environment_get_current_scope() {
        let mut env = Environment::new();
        let span = test_span(1, 1, 1, 5);
        let thunk = Arc::new(Thunk::new_materialized(Value::Int(42), span));
        env.insert("x".into(), Arc::clone(&thunk));

        let found = env.get("x");
        assert!(found.is_some());
        assert!(Arc::ptr_eq(&found.unwrap(), &thunk));
    }

    #[test]
    fn test_environment_get_parent_scope() {
        let mut parent = Environment::new();
        let span = test_span(1, 1, 1, 5);
        let thunk = Arc::new(Thunk::new_materialized(Value::Int(99), span));
        parent.insert("y".into(), Arc::clone(&thunk));

        let parent_rc = Arc::new(RwLock::new(parent));
        let child = Environment::with_parent(Arc::clone(&parent_rc));

        let found = child.get("y");
        assert!(found.is_some());
        assert!(Arc::ptr_eq(&found.unwrap(), &thunk));
    }

    #[test]
    fn test_environment_get_missing() {
        let env = Environment::new();
        assert!(env.get("nonexistent").is_none());
    }

    #[test]
    fn test_environment_get_shadow() {
        let mut parent = Environment::new();
        let span = test_span(1, 1, 1, 5);
        let parent_thunk = Arc::new(Thunk::new_materialized(Value::Int(1), span));
        parent.insert("x".into(), Arc::clone(&parent_thunk));

        let parent_rc = Arc::new(RwLock::new(parent));
        let mut child = Environment::with_parent(parent_rc);
        let child_thunk = Arc::new(Thunk::new_materialized(Value::Int(2), span));
        child.insert("x".into(), Arc::clone(&child_thunk));

        let found = child.get("x").unwrap();
        // Should find the child's binding, not the parent's
        assert!(Arc::ptr_eq(&found, &child_thunk));
        assert!(!Arc::ptr_eq(&found, &parent_thunk));
    }

    #[test]
    fn test_thunk_new_materialized() {
        let span = test_span(1, 1, 1, 5);
        let thunk = Thunk::new_materialized(Value::Int(7), span);
        let val = thunk
            .try_get_materialized()
            .expect("expected Materialized state");
        assert_eq!(val, Value::Int(7));
    }

    #[test]
    fn test_thunk_debug_unevaluated_state() {
        // Verify that Debug output works for an Unevaluated thunk without panicking.
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(0), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let thunk = Thunk::new_unevaluated(expr, env, test_ctx(), span);

        // Debug should not panic for an Unevaluated thunk
        let debug_str = format!("{:?}", thunk);

        // Should contain some indication of the Unevaluated state
        assert!(
            !debug_str.is_empty(),
            "expected non-empty debug output, got empty string"
        );
    }

    #[test]
    fn test_value_display_int() {
        assert_eq!(format!("{}", Value::Int(42)), "42");
        assert_eq!(format!("{}", Value::Int(-10)), "-10");
        assert_eq!(format!("{}", Value::Int(0)), "0");
    }

    #[test]
    fn test_value_display_float() {
        assert_eq!(format!("{}", Value::Float(3.14)), "3.14");
        assert_eq!(format!("{}", Value::Float(-2.5)), "-2.5");
        assert_eq!(format!("{}", Value::Float(0.0)), "0");
    }

    #[test]
    fn test_value_display_string() {
        assert_eq!(format!("{}", string_val("hello")), "\"hello\"");
        assert_eq!(
            format!("{}", string_val("with \"quotes\"")),
            "\"with \\\"quotes\\\"\""
        );
        assert_eq!(format!("{}", string_val("")), "\"\"");
    }

    #[test]
    fn test_value_display_bool() {
        assert_eq!(format!("{}", Value::Bool(true)), "true");
        assert_eq!(format!("{}", Value::Bool(false)), "false");
    }

    #[test]
    fn test_value_display_dict_empty() {
        let dict = Value::Dict(IndexMap::new());
        assert_eq!(format!("{dict}"), "[]");
    }

    #[test]
    fn test_value_display_dict_with_entries() {
        let ctx = test_ctx();
        let mut map: IndexMap<Key, crate::arena::ThunkId> = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        map.insert(
            Key::String("x".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(1), span))),
        );
        map.insert(
            Key::Int(0),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(2), span))),
        );
        let dict = Value::Dict(map);
        assert_eq!(format!("{dict}"), "[x: <thunk> 0: <thunk>]");
    }

    #[test]
    fn test_value_display_function() {
        let span = test_span(1, 1, 1, 5);
        let params = Rc::new(vec![
            Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            },
            Param {
                name: "y".into(),
                annotation: None,
                variadic: false,
            },
        ]);
        let body = Rc::new(Spanned::new(Expr::Int(0), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let func = Value::Function {
            params,
            body,
            env,
            annotation: None,
        };
        assert_eq!(format!("{func}"), "[fn [x y] ...]");
    }

    #[test]
    fn test_value_display_builtin() {
        fn dummy_builtin(
            ctx: BuiltinArgs,
        ) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Int(0),
                    ctx.call_span,
                )))
            })
        }
        let builtin = Value::Builtin(BuiltinDef {
            func: dummy_builtin,
            name: "test_fn",
            pos_strictness: &[],
            force_count: 0,
        });
        assert_eq!(format!("{builtin}"), "<builtin test_fn>");
    }

    #[test]
    fn test_value_debug_int() {
        assert_eq!(format!("{:?}", Value::Int(42)), "Int(42)");
    }

    #[test]
    fn test_value_debug_float() {
        assert_eq!(format!("{:?}", Value::Float(3.14)), "Float(3.14)");
    }

    #[test]
    fn test_value_debug_string() {
        assert_eq!(format!("{:?}", string_val("test")), "String(\"test\")");
    }

    #[test]
    fn test_value_debug_bool() {
        assert_eq!(format!("{:?}", Value::Bool(true)), "Bool(true)");
        assert_eq!(format!("{:?}", Value::Bool(false)), "Bool(false)");
    }

    #[test]
    fn test_value_debug_dict() {
        let ctx = test_ctx();
        let mut map: IndexMap<Key, crate::arena::ThunkId> = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        map.insert(
            Key::String("x".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(1), span))),
        );
        map.insert(
            Key::Int(0),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(2), span))),
        );
        let dict = Value::Dict(map);
        let debug_str = format!("{dict:?}");
        // Dict shows keys only
        assert!(debug_str.starts_with("Dict("));
        assert!(debug_str.contains("String(\"x\")"));
        assert!(debug_str.contains("Int(0)"));
    }

    #[test]
    fn test_value_debug_function() {
        let span = test_span(1, 1, 1, 5);
        let params = Rc::new(vec![
            Param {
                name: "a".into(),
                annotation: None,
                variadic: false,
            },
            Param {
                name: "b".into(),
                annotation: None,
                variadic: false,
            },
        ]);
        let body = Rc::new(Spanned::new(Expr::Int(0), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let func = Value::Function {
            params,
            body,
            env,
            annotation: None,
        };
        assert_eq!(format!("{func:?}"), "Function(a, b)");
    }

    #[test]
    fn test_value_debug_builtin() {
        fn dummy_builtin(
            ctx: BuiltinArgs,
        ) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Int(0),
                    ctx.call_span,
                )))
            })
        }
        let builtin = Value::Builtin(BuiltinDef {
            func: dummy_builtin,
            name: "test_builtin",
            pos_strictness: &[],
            force_count: 0,
        });
        assert_eq!(format!("{builtin:?}"), "Builtin(test_builtin)");
    }

    #[test]
    fn test_thunk_unevaluated_preserves_ctx_across_materialization() {
        use crate::ast::Expr;

        // Create ctx1 with a distinct base_dir
        let base_dir1 = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let env1 = Arc::new(RwLock::new(Environment::new()));
        let ctx1 =
            crate::eval::EvalContext::new(base_dir1, Arc::clone(&env1), Arc::clone(&env1), false);

        // Create a thunk that captures ctx1
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(42), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let thunk =
            Thunk::new_unevaluated(Rc::clone(&expr), Arc::clone(&env), Arc::clone(&ctx1), span);

        // Verify the thunk is in Unevaluated state (peek_expr returns Some)
        assert!(
            thunk.peek_expr().is_some(),
            "thunk should be in Unevaluated state before take_unevaluated"
        );

        // Materialize the thunk using ctx1 (simulating normal evaluation)
        // take_unevaluated atomically transitions to InProgress and returns (expr, env, ctx)
        let taken = thunk.take_unevaluated();
        assert!(
            taken.is_some(),
            "take_unevaluated should succeed on Unevaluated thunk"
        );

        let (_taken_expr, _taken_env, taken_ctx) = taken.unwrap();

        // Verify the taken ctx is the same Arc as ctx1
        assert!(
            Arc::ptr_eq(&taken_ctx, &ctx1),
            "thunk should evaluate using the ctx it captured at creation (ctx1)"
        );

        // Verify that the thunk is now InProgress (after take_unevaluated)
        assert!(
            thunk.is_in_progress(),
            "thunk should be InProgress after take_unevaluated"
        );
    }

    #[test]
    fn test_thunk_pending_builtin_preserves_ctx() {
        fn dummy_builtin(
            ctx: BuiltinArgs,
        ) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Int(0),
                    ctx.call_span,
                )))
            })
        }

        // Create ctx1
        let base_dir1 = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let env1 = Arc::new(RwLock::new(Environment::new()));
        let ctx1 =
            crate::eval::EvalContext::new(base_dir1, Arc::clone(&env1), Arc::clone(&env1), false);

        let span = test_span(1, 1, 1, 5);
        let dummy_def = BuiltinDef {
            func: dummy_builtin,
            name: "test-builtin",
            pos_strictness: &[],
            force_count: 0,
        };
        let thunk = Thunk::new_pending_builtin(
            dummy_def,
            vec![],
            None,
            span,
            Some(Arc::from("test builtin call")),
            Arc::clone(&ctx1),
        );

        // Verify the thunk is in PendingBuiltin state (peek_builtin_def returns Some)
        assert!(
            thunk.peek_builtin_def().is_some(),
            "thunk should be in PendingBuiltin state"
        );

        // Take the pending builtin and verify ctx is preserved
        let taken = thunk.take_pending_builtin();
        assert!(taken.is_some(), "take_pending_builtin should succeed");

        let (_def, _args, _named, _call_span, taken_ctx) = taken.unwrap();
        assert!(
            Arc::ptr_eq(&taken_ctx, &ctx1),
            "PendingBuiltin should evaluate using captured ctx1"
        );
    }

    #[test]
    fn test_thunk_pending_call_preserves_ctx() {
        // Create ctx1
        let base_dir1 = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let env1 = Arc::new(RwLock::new(Environment::new()));
        let ctx1 =
            crate::eval::EvalContext::new(base_dir1, Arc::clone(&env1), Arc::clone(&env1), false);

        let span = test_span(1, 1, 1, 5);
        let func_thunk = Arc::new(Thunk::new_materialized(
            Value::Function {
                params: Rc::new(vec![]),
                body: Rc::new(Spanned::new(
                    crate::ast::Expr::Int(0),
                    test_span(1, 1, 1, 1),
                )),
                env: Arc::new(RwLock::new(Environment::new())),
                annotation: None,
            },
            span,
        ));

        let thunk = Thunk::new_pending_call(
            Arc::clone(&func_thunk),
            vec![],
            IndexMap::new(),
            span,
            Arc::new(RwLock::new(Environment::new())), // caller_env
            span,
            Some(Arc::from("test call")),
            Arc::clone(&ctx1),
        );

        // Verify the thunk is in PendingCall state
        assert!(
            thunk.is_pending_call(),
            "thunk should be in PendingCall state"
        );

        // Take the pending call and verify ctx is preserved
        let taken = thunk.take_pending_call();
        assert!(taken.is_some(), "take_pending_call should succeed");

        let (_func, _args, _named, _call_span, _caller_env, taken_ctx) = taken.unwrap();
        assert!(
            Arc::ptr_eq(&taken_ctx, &ctx1),
            "PendingCall should evaluate using captured ctx1"
        );
    }

    #[test]
    fn test_strkey_lookup() {
        // Task 6: Test StrKey hash/equivalent for zero-allocation lookups
        let mut map: IndexMap<Key, i32> = IndexMap::new();
        map.insert(Key::String("foo".into()), 42);
        map.insert(Key::String("bar".into()), 99);
        map.insert(Key::Int(0), 100);

        // Positive case: lookup with StrKey should work
        assert_eq!(map.get(&StrKey("foo")), Some(&42));
        assert_eq!(map.get(&StrKey("bar")), Some(&99));

        // Negative case: non-matching key should return None
        assert_eq!(map.get(&StrKey("baz")), None);

        // StrKey should not match Int keys
        assert_eq!(map.get(&StrKey("0")), None);
    }

    #[test]
    fn test_proxy_type_name() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 1);
        let proxy = Value::Proxy {
            handler: ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(42), span))),
        };
        assert_eq!(proxy.type_name(), "Proxy");
    }

    #[test]
    fn test_proxy_debug() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 1);
        let proxy = Value::Proxy {
            handler: ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(42), span))),
        };
        let debug_str = format!("{:?}", proxy);
        assert_eq!(debug_str, "Proxy");
    }

    #[test]
    fn test_proxy_display() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 1);
        let proxy = Value::Proxy {
            handler: ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(42), span))),
        };
        let display_str = format!("{}", proxy);
        assert_eq!(display_str, "<proxy>");
    }

    #[test]
    fn test_value_partial_eq_proxy_always_false() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 1);
        let p = Value::Proxy {
            handler: ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Int(42), span))),
        };
        assert_ne!(p.clone(), p);
    }

    #[test]
    fn test_thunk_new_guarded_state() {
        let span = test_span(1, 1, 1, 5);
        let inner = Arc::new(Thunk::new_materialized(Value::Int(42), span));
        let thunk = Thunk::new_guarded(
            Arc::clone(&inner),
            Type::Int,
            vec!["field".to_string()],
            span,
        );
        assert!(
            thunk.is_guarded(),
            "expected Guarded state (is_guarded should return true)"
        );
        // Verify the components by taking them
        let taken = thunk.take_guarded();
        assert!(taken.is_some(), "should be able to take guarded state");
        let (_inner, expected, field_path, _span, _blame, _default) = taken.unwrap();
        assert_eq!(expected, Type::Int);
        assert_eq!(field_path, vec!["field".to_string()]);
    }

    #[test]
    fn test_take_guarded_returns_components() {
        let span = test_span(1, 1, 1, 5);
        let inner = Arc::new(Thunk::new_materialized(Value::Int(99), span));
        let thunk = Thunk::new_guarded(Arc::clone(&inner), Type::Int, vec!["x".to_string()], span);

        let result = thunk.take_guarded();
        assert!(
            result.is_some(),
            "take_guarded should succeed on Guarded thunk"
        );

        let (taken_inner, taken_expected, taken_path, _taken_span, _blame, taken_default) =
            result.unwrap();
        assert!(
            Arc::ptr_eq(&taken_inner, &inner),
            "inner thunk should be the same Rc"
        );
        assert_eq!(taken_expected, Type::Int);
        assert_eq!(taken_path, vec!["x".to_string()]);
        assert!(
            taken_default.is_none(),
            "default should be None when not provided"
        );

        // After take_guarded, thunk should be InProgress
        assert!(
            thunk.is_in_progress(),
            "expected InProgress after take_guarded"
        );
    }

    #[test]
    fn test_take_guarded_on_non_guarded_returns_none() {
        let span = test_span(1, 1, 1, 5);
        let thunk = Thunk::new_materialized(Value::Int(7), span);

        let result = thunk.take_guarded();
        assert!(
            result.is_none(),
            "take_guarded on Materialized thunk should return None"
        );

        // State should be unchanged (still Materialized)
        assert_eq!(
            thunk.try_get_materialized(),
            Some(Value::Int(7)),
            "expected Materialized state to be preserved"
        );
    }

    #[test]
    fn test_thunk_new_guarded_fields() {
        let span = test_span(1, 1, 1, 5);
        let inner = Arc::new(Thunk::new_materialized(Value::Int(42), span));
        let thunk = Arc::new(Thunk::new_guarded(
            Arc::clone(&inner),
            Type::Int,
            vec!["foo".to_string()],
            span,
        ));
        let result = thunk.take_guarded();
        assert!(result.is_some());
        let (got_inner, got_type, got_path, _got_span, _blame, _default) = result.unwrap();
        assert_eq!(got_path, vec!["foo".to_string()]);
        assert!(matches!(got_type, Type::Int));
        assert!(Arc::ptr_eq(&got_inner, &inner));
    }

    #[test]
    fn test_guarded_materialized_state_is_stable() {
        // Verifies that once a Guarded thunk is transitioned to Materialized,
        // the state is stable on re-access. Tests the state machine directly;
        // the full guard validation path (parse→eval→materialize) is covered
        // by test_guarded_thunk_preserves_inner_origin in eval.rs.
        let span = test_span(1, 1, 1, 5);
        let inner = Arc::new(Thunk::new_materialized(Value::Int(100), span));
        let thunk = Thunk::new_guarded(Arc::clone(&inner), Type::Int, vec![], span);

        // Verify initial state is Guarded
        assert!(thunk.is_guarded(), "initial state should be Guarded");

        // Directly transition to Materialized to verify state is stable on re-access.
        thunk.set_materialized(Value::Int(100));

        // Re-access: should return cached Materialized value
        assert_eq!(
            thunk.try_get_materialized(),
            Some(Value::Int(100)),
            "expected Materialized after guard success"
        );

        // try_get_materialized should also work
        let cached = thunk.try_get_materialized();
        assert_eq!(cached, Some(Value::Int(100)));
    }

    #[test]
    fn test_pending_builtin_lifecycle() {
        // Verify PendingBuiltin thunk can be created and transitions correctly
        use crate::eval::EvalContext;
        use crate::test_util::test_span;

        let span = test_span(1, 1, 1, 10);
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let env = Arc::new(RwLock::new(Environment::new()));
        let ctx = EvalContext::new(base_dir, Arc::clone(&env), Arc::clone(&env), false);

        // Create a PendingBuiltin thunk (using a dummy builtin function)
        fn dummy_builtin(
            args: BuiltinArgs,
        ) -> Pin<Box<dyn Future<Output = crate::error::EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let _ = args; // silence unused warning
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Int(42),
                    test_span(1, 1, 1, 1),
                )))
            })
        }

        let dummy_def = BuiltinDef {
            func: dummy_builtin,
            name: "dummy",
            pos_strictness: &[],
            force_count: 0,
        };
        let thunk = Thunk::new_pending_builtin(
            dummy_def,
            vec![],
            None,
            span,
            Some(Arc::from("test")),
            Arc::clone(&ctx),
        );

        // Verify initial state is PendingBuiltin
        assert!(
            thunk.peek_builtin_def().is_some(),
            "initial state should be PendingBuiltin"
        );

        // Transition to Materialized
        thunk.set_materialized(Value::Int(42));

        // Verify final state is Materialized
        assert_eq!(
            thunk.try_get_materialized(),
            Some(Value::Int(42)),
            "expected Materialized after builtin execution"
        );
    }

    #[test]
    fn test_pending_builtin_error_recovery() {
        // Verify PendingBuiltin thunk transitions to Failed state on error
        use crate::error::EvalError;
        use crate::eval::EvalContext;
        use crate::test_util::test_span;

        let span = test_span(1, 1, 1, 10);
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let env = Arc::new(RwLock::new(Environment::new()));
        let ctx = EvalContext::new(base_dir, Arc::clone(&env), Arc::clone(&env), false);

        fn error_builtin(
            args: BuiltinArgs,
        ) -> Pin<Box<dyn Future<Output = crate::error::EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                Err(Box::new(EvalError::internal(
                    "test error".into(),
                    args.call_span,
                )))
            })
        }

        let error_def = BuiltinDef {
            func: error_builtin,
            name: "error_builtin",
            pos_strictness: &[],
            force_count: 0,
        };
        let thunk = Thunk::new_pending_builtin(
            error_def,
            vec![],
            None,
            span,
            Some(Arc::from("test")),
            Arc::clone(&ctx),
        );

        // Transition to Failed
        let err = EvalError::internal("test error".into(), span);
        thunk.cache_failure(&err);

        // Verify final state is Failed
        let cached = thunk.get_cached_error();
        assert!(cached.is_some(), "expected Failed state");
        assert!(
            cached.unwrap().kind.to_string().contains("test error"),
            "cached error should contain 'test error'"
        );
    }

    #[test]
    fn test_string_val_helper() {
        let s = string_val("hello");
        assert_eq!(s.as_str(), Some("hello"));
        assert_eq!(format!("{s}"), "\"hello\"");
        assert_eq!(format!("{s:?}"), "String(\"hello\")");
    }

    #[test]
    fn test_string_val_empty() {
        let s = string_val("");
        assert_eq!(s.as_str(), Some(""));
        assert_eq!(format!("{s}"), "\"\"");
    }

    #[test]
    fn test_string_equality() {
        // Same content, different Rc instances
        let s1 = string_val("test");
        let s2 = string_val("test");
        assert_eq!(s1, s2);

        // Different content
        let s3 = string_val("other");
        assert_ne!(s1, s3);
    }

    #[test]
    fn test_as_str_on_non_string() {
        assert_eq!(Value::Int(42).as_str(), None);
        assert_eq!(Value::Bool(true).as_str(), None);
        assert_eq!(Value::Float(3.14).as_str(), None);
    }

    // --- get_cached_error() contract ---

    #[test]
    fn test_get_cached_error_failed_returns_some() {
        // Failed thunk: cache_failure() sets the error; get_cached_error() must return Some.
        // Start from Unevaluated so the OnceCell result is unset, then transition to Failed.
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(0), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let thunk = Thunk::new_unevaluated(expr, env, test_ctx(), span);
        let err = crate::error::EvalError::internal("sentinel error".into(), span);
        thunk.cache_failure(&err);

        let result = thunk.get_cached_error();
        assert!(
            result.is_some(),
            "get_cached_error() must return Some for Failed thunk"
        );
        let got = result.unwrap();
        // Error identity: the message matches the one we put in.
        assert!(
            got.kind.to_string().contains("sentinel error"),
            "returned error should contain 'sentinel error', got: {}",
            got.kind
        );
    }

    #[test]
    fn test_get_cached_error_materialized_returns_none() {
        let span = test_span(1, 1, 1, 5);
        let thunk = Thunk::new_materialized(Value::Int(42), span);
        assert!(
            thunk.get_cached_error().is_none(),
            "get_cached_error() must return None for Materialized thunk"
        );
    }

    #[test]
    fn test_get_cached_error_unevaluated_returns_none() {
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(0), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let thunk = Thunk::new_unevaluated(expr, env, test_ctx(), span);
        assert!(
            thunk.get_cached_error().is_none(),
            "get_cached_error() must return None for Unevaluated thunk"
        );
    }

    #[test]
    fn test_get_cached_error_in_progress_returns_none() {
        // InProgress: take_unevaluated() transitions to InProgress; result not yet set.
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(0), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let thunk = Thunk::new_unevaluated(expr, env, test_ctx(), span);
        let _taken = thunk.take_unevaluated(); // transitions to InProgress
        assert!(
            thunk.get_cached_error().is_none(),
            "get_cached_error() must return None for InProgress thunk"
        );
    }

    // --- is_in_progress() contract ---

    #[test]
    fn test_is_in_progress_true_after_take_unevaluated() {
        // After take_unevaluated(), thunk is InProgress: is_in_progress() must return true.
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(0), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let thunk = Thunk::new_unevaluated(expr, env, test_ctx(), span);
        thunk.take_unevaluated(); // transitions to InProgress
        assert!(
            thunk.is_in_progress(),
            "is_in_progress() must return true after take_unevaluated()"
        );
    }

    #[test]
    fn test_is_in_progress_false_for_unevaluated() {
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(0), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let thunk = Thunk::new_unevaluated(expr, env, test_ctx(), span);
        assert!(
            !thunk.is_in_progress(),
            "is_in_progress() must return false for Unevaluated thunk"
        );
    }

    #[test]
    fn test_is_in_progress_false_for_materialized() {
        let span = test_span(1, 1, 1, 5);
        let thunk = Thunk::new_materialized(Value::Int(7), span);
        assert!(
            !thunk.is_in_progress(),
            "is_in_progress() must return false for Materialized thunk"
        );
    }

    #[test]
    fn test_is_in_progress_false_for_failed() {
        // Start from Unevaluated so set_state(Failed) can actually write to the OnceCell.
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(0), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let thunk = Thunk::new_unevaluated(expr, env, test_ctx(), span);
        let err = crate::error::EvalError::internal("test".into(), span);
        thunk.cache_failure(&err);
        assert!(
            !thunk.is_in_progress(),
            "is_in_progress() must return false for Failed thunk"
        );
    }

    #[test]
    fn test_is_in_progress_false_for_guarded() {
        let span = test_span(1, 1, 1, 5);
        let inner = Arc::new(Thunk::new_materialized(Value::Int(42), span));
        let thunk = Thunk::new_guarded(Arc::clone(&inner), Type::Int, vec![], span);
        assert!(
            !thunk.is_in_progress(),
            "is_in_progress() must return false for Guarded thunk"
        );
    }

    // --- Thunk sequential access test ---

    #[test]
    fn test_thunk_sequential_materialized_access() {
        // Verify that two separate materialized thunks can be accessed sequentially
        // without interference. In the new Mutex-based ThunkInner design, each thunk
        // holds its own Mutex so there is no shared-lock contention between distinct thunks.
        let span = test_span(1, 1, 1, 5);
        let thunk1 = Thunk::new_materialized(Value::Int(1), span);
        let thunk2 = Thunk::new_materialized(Value::Int(2), span);

        // First access: verify thunk1 is materialized.
        assert_eq!(
            thunk1.try_get_materialized(),
            Some(Value::Int(1)),
            "expected Materialized(Int(1))"
        );

        // Second access: verify thunk2 is still accessible (no cross-thunk locking issues).
        assert_eq!(
            thunk2.try_get_materialized(),
            Some(Value::Int(2)),
            "expected Materialized(Int(2))"
        );
    }
}
