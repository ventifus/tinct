//! Runtime value types: `Value`, `Thunk` (lazy memoization), `Scope` (closure-converted EvalFrame-based variable lookup).

use std::fmt;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use crate::ast::{CoreExpr, Param, Span, Spanned, SurfaceDocument, SurfaceNode};
use crate::error::{EvalError, EvalResult};

/// Type alias for the optional default expression + environment pair in guarded thunks.
/// Reduces type_complexity in UnevaluatedState::Guarded and Thunk constructors.
/// `env_id` is the caller's FlatEnv identity, used for scope resolution during default evaluation.
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
    pub args: Vec<std::sync::Arc<Thunk>>,
    pub named: Option<indexmap::IndexMap<String, std::sync::Arc<Thunk>>>,
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
/// `+ Send` is required: builtins run on the multi-threaded tokio runtime. All captured
/// types (`Value`, `Arc<Thunk>`, `Arc<EvalContext>`) are Send after T-1768 (Value: Send)
/// and T-1774 (ScopeArena eliminated). The h3 driver task in builtins_net.rs uses
/// `spawn_local` for its `!Send` h3::client::Connection — that is the sole remaining
/// LocalSet use and is handled by keeping `spawn_local` only for that site.
pub type BuiltinFn =
    fn(BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>>;

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

/// Persistent append-only cons-list for LGM slot resolution in EvalFrame.
///
/// Each spine node holds the entries added at one dict boundary and a shared
/// pointer to the previous level. Creating a new EvalFrame is O(1) Arc::clone;
/// extending after a dict evaluates is O(|new entries|); get(slot) is O(depth)
/// where depth ≤ number of dicts in the current document (≤ ~5 in practice).
pub struct GroupSpine {
    entries: std::sync::Arc<[std::sync::Arc<Thunk>]>,
    offset: usize,
    len: usize,
    prev: Option<std::sync::Arc<GroupSpine>>,
}

impl std::fmt::Debug for GroupSpine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GroupSpine(len={})", self.len)
    }
}

impl GroupSpine {
    pub fn empty() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            entries: std::sync::Arc::from(vec![]),
            offset: 0,
            len: 0,
            prev: None,
        })
    }

    pub fn from_flat(entries: Vec<std::sync::Arc<Thunk>>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            len: entries.len(),
            entries: entries.into(),
            offset: 0,
            prev: None,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, slot: usize) -> Option<std::sync::Arc<Thunk>> {
        if slot >= self.offset {
            self.entries
                .get(slot - self.offset)
                .map(std::sync::Arc::clone)
        } else {
            self.prev.as_ref()?.get(slot)
        }
    }

    pub fn extend(
        self: &std::sync::Arc<Self>,
        new_entries: Vec<std::sync::Arc<Thunk>>,
    ) -> std::sync::Arc<Self> {
        if new_entries.is_empty() {
            return std::sync::Arc::clone(self);
        }
        let offset = self.len;
        std::sync::Arc::new(Self {
            len: offset + new_entries.len(),
            entries: new_entries.into(),
            offset,
            prev: Some(std::sync::Arc::clone(self)),
        })
    }
}

/// Replaces ScopeArena-based scope chain traversal.
///
/// - `closure_env[i]`: thunk for `VarAddr::ClosureCapture(i)` references (fn captures only)
/// - `group[i]`: thunk for `VarAddr::LetrecGroupMember(i)` references.
///   At document level, `group` is the accumulated_group: root-scope entries at slots 0..N-1,
///   followed by each dict's entries at cumulative slot offsets. No outer-frame traversal needed.
/// - `params[i]`: thunk for `VarAddr::Parameter(i)` references
///
/// All three VarAddr variants index directly into this frame's vectors.
/// No outer-frame chain traversal — cross-scope references are resolved at the resolver level
/// (captures become ClosureCapture; document cross-dict refs become LGM with cumulative slots).
#[derive(Debug, Clone)]
pub struct EvalFrame {
    pub closure_env: std::sync::Arc<GroupSpine>,
    pub group: std::sync::Arc<GroupSpine>,
    pub params: std::sync::Arc<Vec<std::sync::Arc<Thunk>>>,
}

impl EvalFrame {
    pub fn empty() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            closure_env: GroupSpine::empty(),
            group: GroupSpine::empty(),
            params: std::sync::Arc::new(vec![]),
        })
    }

    pub fn for_function_call(
        closure_env: std::sync::Arc<Vec<std::sync::Arc<Thunk>>>,
        params: Vec<std::sync::Arc<Thunk>>,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            closure_env: GroupSpine::from_flat(closure_env.iter().cloned().collect()),
            group: GroupSpine::empty(),
            params: std::sync::Arc::new(params),
        })
    }
}

/// Dict key type: any fully-materialised tinct value that implements Hash + Eq.
/// This is the canonical hashable key used in `Value::Dict` and `IndexMap<HashableValue, Arc<Thunk>>`.
/// Only these Value variants may appear as dict keys. Function, Handle, Task, and Builder
/// cannot be hashed (no structural identity).
#[derive(Debug, Clone)]
pub enum HashableValue {
    Int(i64),
    Str(Arc<str>),
    /// Float key stored as raw IEEE 754 bits (u64) for bitwise equality and hashing.
    /// TotalF64 semantics: NaN == NaN iff both have the same bit pattern. Two floats that
    /// represent different bit patterns are unequal as keys even if they would compare equal
    /// under IEEE 754 (e.g. -0.0 and +0.0 have different bits and are distinct keys).
    /// This is sound for HashMap semantics: reflexive, symmetric, transitive.
    Float(u64),
    /// Pairs in insertion order. Equality is order-insensitive.
    Dict(Vec<(HashableValue, HashableValue)>),
    Variant {
        tag: Arc<str>,
        payload: Option<Box<HashableValue>>,
    },
}

/// Splitmix64 mix function: non-linear bijection for combining hash values.
/// Used for order-insensitive Dict hashing — the commutative sum of mix(hash(k), hash(v))
/// for each (k,v) pair ensures insertion order does not affect the hash.
/// Non-linearity prevents key-value-swap collisions.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

impl PartialEq for HashableValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (HashableValue::Int(a), HashableValue::Int(b)) => a == b,
            (HashableValue::Str(a), HashableValue::Str(b)) => a == b,
            // Bitwise equality: Float(a) == Float(b) iff their bit representations are identical.
            // This makes NaN == NaN when both have the same bit pattern (TotalF64 semantics).
            (HashableValue::Float(a), HashableValue::Float(b)) => a == b,
            (HashableValue::Dict(a), HashableValue::Dict(b)) => {
                if a.len() != b.len() {
                    return false;
                }
                // Order-insensitive: build a HashMap from `a`, then compare key-by-key.
                // Uses explicit insertion loop instead of collect() to handle duplicate
                // keys safely (returns false instead of panicking).
                let mut map =
                    std::collections::HashMap::<&HashableValue, &HashableValue>::with_capacity(
                        a.len(),
                    );
                for (k, v) in a {
                    if map.insert(k, v).is_some() {
                        // Duplicate key in `a` — not a valid Dict, can't be equal
                        return false;
                    }
                }
                b.iter().all(|(k, v)| map.get(k).map_or(false, |u| *u == v))
            }
            (
                HashableValue::Variant {
                    tag: t1,
                    payload: p1,
                },
                HashableValue::Variant {
                    tag: t2,
                    payload: p2,
                },
            ) => t1 == t2 && p1 == p2,
            _ => false, // different variants or cross-type
        }
    }
}

impl Eq for HashableValue {}

impl PartialOrd for HashableValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (HashableValue::Int(a), HashableValue::Int(b)) => a.partial_cmp(b),
            (HashableValue::Str(a), HashableValue::Str(b)) => a.partial_cmp(b),
            // Float keys are ordered by their bit representation (not by f64 value order).
            // This is not meaningful as a numeric ordering, but provides a total order
            // over Float keys so that IndexMap can sort them if needed.
            (HashableValue::Float(a), HashableValue::Float(b)) => a.partial_cmp(b),
            _ => None, // mixed types and complex types are incomparable
        }
    }
}

impl Hash for HashableValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Use explicit u8 discriminants instead of std::mem::discriminant.
        // Discriminants: Int=0, Str=2, Dict=3, Variant=4, Float=5
        match self {
            HashableValue::Int(n) => {
                0u8.hash(state);
                n.hash(state);
            }
            HashableValue::Str(s) => {
                2u8.hash(state);
                s.hash(state);
            }
            HashableValue::Float(bits) => {
                5u8.hash(state);
                bits.hash(state);
            }
            HashableValue::Dict(pairs) => {
                3u8.hash(state);
                // Order-insensitive: commutative sum of mix(hash(k), hash(v))
                // using splitmix64 as the non-linear mixer.
                let mut sum: u64 = 0;
                for (k, v) in pairs {
                    let mut kh = std::collections::hash_map::DefaultHasher::new();
                    k.hash(&mut kh);
                    let key_hash = kh.finish();

                    let mut vh = std::collections::hash_map::DefaultHasher::new();
                    v.hash(&mut vh);
                    let val_hash = vh.finish();

                    sum = sum.wrapping_add(splitmix64(key_hash.wrapping_add(val_hash)));
                }
                sum.hash(state);
            }
            HashableValue::Variant { tag, payload } => {
                4u8.hash(state);
                tag.hash(state);
                match payload {
                    None => 0u8.hash(state),
                    Some(v) => {
                        1u8.hash(state);
                        v.hash(state);
                    }
                }
            }
        }
    }
}

impl fmt::Display for HashableValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HashableValue::Int(n) => write!(f, "{n}"),
            HashableValue::Str(s) => write!(f, "{s}"),
            HashableValue::Float(bits) => write!(f, "{}", f64::from_bits(*bits)),
            HashableValue::Dict(pairs) => {
                write!(f, "[")?;
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        write!(f, "  ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "]")
            }
            HashableValue::Variant { tag, payload } => match payload {
                None => write!(f, "{tag}"),
                Some(p) => write!(f, "[{tag} {p}]"),
            },
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

    /// Freeze the builder without consuming the inner map. Returns Err if already frozen.
    ///
    /// Unlike `finish()`, this does not extract the map — use it when the intent is only
    /// to lock the builder against further mutations (e.g., sentinel values that are never
    /// read back). Calling `finish()` when only freezing is needed violates the API contract
    /// of `finish()`, which is "take the map".
    pub fn freeze(&self) -> Result<(), String> {
        if self.frozen.swap(true, Ordering::Relaxed) {
            Err("builder is already frozen".to_string())
        } else {
            Ok(())
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

/// Bootstrap meta-sentinel used as the `type_val` of `unknown_type_val()`.
///
/// The unknown sentinel is an empty `Value::Dict`. Every Dict carries a `type_val: Arc<Value>`,
/// so creating the dict requires a pre-existing TypeValue. The meta-sentinel is a frozen
/// `Value::Builder` — the only Value variant that carries no `type_val` field — used solely
/// to break the circularity at this one bootstrap site. It is never returned to user code
/// and is never passed through `llt-repr` or `to_tinct`.
fn meta_type_sentinel() -> Arc<Value> {
    use std::sync::OnceLock;
    static META: OnceLock<Arc<Value>> = OnceLock::new();
    Arc::clone(META.get_or_init(|| {
        let b = Builder::new();
        // Freeze immediately — this Builder is used only as a circularity-breaker for the
        // unknown_type_val() bootstrap sentinel. It is never mutated or read; frozen state
        // prevents any future inserts. Use freeze() not finish(): finish() extracts the inner
        // map (destructive), which is not the intent here.
        b.freeze().expect("fresh builder must be freezable");
        Arc::new(Value::Builder(Arc::new(b)))
    }))
}

/// Sentinel TypeValue for values whose runtime type is not yet known.
/// Returns a static Arc<Value> — the bootstrap TypeValue.Unknown sentinel.
///
/// Represented as an empty `Value::Dict` so that `llt-repr` formats it as `[]`,
/// matching the design intent "TypeValue.Unknown = empty dict during bootstrap".
/// The dict's own `type_val` is the `meta_type_sentinel()` (a frozen Builder), which
/// breaks the circularity: Dict needs a type_val, Builder has none.
///
/// Used as the default `type_val` on every Value variant during bootstrap, before
/// `repr:` declarations wire up real TypeValues.
pub fn unknown_type_val() -> Arc<Value> {
    use std::sync::OnceLock;
    static UNKNOWN: OnceLock<Arc<Value>> = OnceLock::new();
    Arc::clone(UNKNOWN.get_or_init(|| {
        Arc::new(Value::Dict {
            entries: indexmap::IndexMap::new(),
            type_val: meta_type_sentinel(),
        })
    }))
}

/// Returns a static reference to the unknown type_val sentinel.
///
/// Same singleton as `unknown_type_val()` but returned as `&'static Arc<Value>` instead
/// of a fresh `Arc` clone — useful in methods that need to return `&Arc<Value>` without
/// allocating (e.g., `Value::type_val()` for the `Builder` and fallback arms).
pub fn unknown_type_val_ref() -> &'static Arc<Value> {
    use std::sync::OnceLock;
    static UNKNOWN_REF: OnceLock<Arc<Value>> = OnceLock::new();
    UNKNOWN_REF.get_or_init(unknown_type_val)
}

/// Extract the type constructor name from a qualified constructor name.
///
/// Given a `ctor` field like `"Color.Red"`, returns `"Color"`.
/// Given an unqualified name like `"Red"`, returns `"Red"` unchanged.
///
/// Used wherever code previously read `variant.tycon` to determine the type
/// constructor name for type checking or display purposes.
pub fn tycon_name_from_ctor(ctor: &str) -> &str {
    ctor.split('.').next().unwrap_or(ctor)
}

/// A materialized runtime value.
pub enum Value {
    /// 64-bit signed integer
    Int { n: i64, type_val: Arc<Value> },
    /// Unsigned 64-bit integer (from `42u`, `0xFFu` literals)
    U64 { n: u64, type_val: Arc<Value> },
    /// 64-bit IEEE 754 float
    Float { n: f64, type_val: Arc<Value> },
    /// UTF-8 string (from bare words or quoted literals).
    /// Stored as a shared slice of a source string with byte offsets.
    /// This enables zero-copy substring operations and shared storage.
    String {
        source: Arc<str>,
        start: usize,
        end: usize,
        type_val: Arc<Value>,
    },
    /// Ordered key-value map with lazy (thunked) values
    Dict {
        entries: IndexMap<HashableValue, Arc<Thunk>>,
        type_val: Arc<Value>,
    },
    /// Transient accumulator — no type_val. Consumed before type identity matters.
    ///
    /// Mutable dict builder: one-shot invariant (once frozen via builder-finish, all mutations
    /// error). Sequential-use: not safe for concurrent modification (Mutex protects state, not
    /// semantics). Also serves as the `unknown_type_val()` bootstrap sentinel — `Value::Builder`
    /// breaks the circularity that would arise from requiring a `type_val` to construct any Value.
    Builder(Arc<Builder>),
    /// User-defined function (closure capturing its defining environment).
    /// `body` is stored as `Arc<Spanned<CoreExpr>>` (Parts-E migration: no Expr round-trip).
    /// `closure_env` is the captured variable vector for closure-converted lookup.
    /// All cross-scope captures are resolved at fn-creation time into `closure_env`;
    /// no outer-frame pointer is needed at call time.
    Function {
        params: Arc<Vec<Param>>,
        body: Arc<Spanned<CoreExpr>>,
        closure_env: std::sync::Arc<Vec<std::sync::Arc<Thunk>>>,
        annotation: Option<Box<FnAnnotation>>,
        type_val: Arc<Value>,
    },
    /// Rust-native built-in function
    Builtin {
        def: BuiltinDef,
        type_val: Arc<Value>,
    },
    /// Proxy object — field access calls the handler function with the field name
    Proxy {
        handler: std::sync::Arc<Thunk>,
        type_val: Arc<Value>,
    },
    /// Capability-bound directory handle (object capability model)
    DirCap {
        dir: cap_std::fs::Dir,
        perms: DirPerms,
        type_val: Arc<Value>,
    },
    /// Network capability — authority to connect to specified hosts/subnets
    NetCap {
        entries: Arc<Vec<NetCapEntry>>,
        type_val: Arc<Value>,
    },
    /// Raw OS file handle (thin wrapper over cap_std::fs::File, no buffering).
    /// Opened via `builtin-file-open`; read/written/sought via `builtin-file-*` builtins.
    File {
        inner: Arc<Mutex<cap_std::fs::File>>,
        type_val: Arc<Value>,
    },
    /// Revocable directory capability
    RevocableDirCap {
        inner: cap_std::fs::Dir,
        perms: DirPerms,
        revoked: Arc<AtomicBool>,
        type_val: Arc<Value>,
    },
    /// Nominal variant (enum-like value)
    Variant {
        /// The runtime TypeValue for this variant's type constructor.
        /// Set to `unknown_type_val()` at construction sites that do not yet have
        /// a resolved TypeValue; wired up by the repr: protocol when it runs.
        type_val: Arc<Value>,
        ctor: Arc<str>,
        payload: Option<std::sync::Arc<Thunk>>,
    },
    /// Exact base-10 decimal (rust_decimal::Decimal, 96-bit software decimal).
    Decimal {
        n: rust_decimal::Decimal,
        type_val: Arc<Value>,
    },
    /// Arbitrary-precision integer (num_bigint::BigInt).
    BigInt {
        n: num_bigint::BigInt,
        type_val: Arc<Value>,
    },
    /// Byte sequence (opaque binary data).
    Bytes {
        source: Arc<[u8]>,
        start: usize,
        end: usize,
        type_val: Arc<Value>,
    },
    /// URI — a uniform resource identifier with scheme and URI string.
    Uri {
        scheme: String,
        uri: String,
        type_val: Arc<Value>,
    },
    /// UTC timestamp — pre-validated jiff::Timestamp. Construction must succeed at creation
    /// sites (all i64-nanosecond values are within jiff's representable range).
    Timestamp {
        ts: jiff::Timestamp,
        type_val: Arc<Value>,
    },
    /// Signed duration (nanoseconds).
    Duration { nanos: i64, type_val: Arc<Value> },
    /// Clock capability for reading current time (object capability model).
    ClockCap {
        inner: Arc<ClockCapInner>,
        type_val: Arc<Value>,
    },
    /// Timezone (parsed IANA TZ rules from zoneinfo file).
    Timezone {
        tz: Arc<jiff::tz::TimeZone>,
        type_val: Arc<Value>,
    },
    /// QUIC session — multiplexed connection over UDP (RFC 9000).
    QuicSession {
        conn: Arc<quinn::Connection>,
        type_val: Arc<Value>,
    },
    /// HTTP/2 session — multiplexed HTTP connection (RFC 9113).
    Http2Session {
        client: Arc<reqwest::Client>,
        base_url: String,
        type_val: Arc<Value>,
    },
    /// HTTP/3 session — HTTP over QUIC (RFC 9114).
    Http3Session {
        session: Arc<Mutex<Http3SessionState>>,
        type_val: Arc<Value>,
    },
    /// QUIC datagram handle — unreliable message delivery over QUIC (RFC 9221).
    QuicDatagramHandle {
        conn: Arc<quinn::Connection>,
        type_val: Arc<Value>,
    },

    // =========================================================================
    // runtime-v2 native AST value types
    // =========================================================================
    /// A complete tinct program — the type returned by `builtin-parse` and related builtins.
    ///
    /// The `SurfaceProgram` AST is stored directly in an `Arc` for shared ownership.
    /// `resolutions` is populated by the resolve pipeline stage and carried alongside
    /// the program for use by downstream builtins.
    Program {
        program: std::sync::Arc<crate::ast::SurfaceProgram>,
        resolutions: Arc<crate::ast::ResolutionTable>,
        type_val: Arc<Value>,
    },

    /// A single document within a program — accessible via `program.documents`.
    Document {
        doc: Arc<SurfaceDocument>,
        /// Unified scope frames from `resolve_surface_document_with_seed_frames`.
        ///
        /// Contains all scope frames collected during resolution — both Dict letrec frames
        /// and BlockBody sequential injection frames, in injection order. Used by the type
        /// checker (typecheck_cek.rs) for slot base lookup and by `builtin-lower`
        /// (make_method_dispatcher_fn) for mangled instance binding name resolution.
        ///
        /// Set by builtin-resolve. Empty if the document was not resolved.
        resolver_frames: Arc<Vec<(indexmap::IndexMap<String, u32>, crate::resolve::FrameKind)>>,
        type_val: Arc<Value>,
    },

    /// A single AST expression node — the type returned by `ast-of` and `[quote ...]`.
    Expression {
        node: Arc<SurfaceNode>,
        type_val: Arc<Value>,
    },

    // =========================================================================
    // runtime-v2 async primitives
    // =========================================================================
    /// Async task handle — returned by `task` builtin, consumed by `await`.
    Task {
        state: Arc<tokio::sync::Mutex<TaskState>>,
        type_val: Arc<Value>,
    },

    /// Channel for inter-task communication — created by `channel` builtin.
    Channel {
        inner: Arc<ChannelInner>,
        type_val: Arc<Value>,
    },

    /// Broadcast channel — created by `broadcast-channel` builtin.
    BroadcastChannel {
        inner: Arc<BroadcastChannelInner>,
        type_val: Arc<Value>,
    },

    /// Oneshot sender half — created by `oneshot-channel` builtin.
    OneshotSender {
        inner: Arc<OneshotSenderInner>,
        type_val: Arc<Value>,
    },

    /// Oneshot receiver half — created by `oneshot-channel` builtin.
    OneshotReceiver {
        inner: Arc<OneshotReceiverInner>,
        type_val: Arc<Value>,
    },

    /// Cancellation context — created by `context` builtin.
    Context {
        token: tokio_util::sync::CancellationToken,
        type_val: Arc<Value>,
    },

    /// Reactive cell — created by `reactive-cell` builtin.
    ReactiveCell {
        inner: Arc<ReactiveCellInner>,
        type_val: Arc<Value>,
    },

    /// Arena view handle — wraps a named scope managed by this arena.
    /// `start_env_id` is the root scope allocated by `arena-new`.
    /// The actual end of the arena is always computed dynamically from `envs.len()` at
    /// drop/migrate time; there is no stored end field (it would be stale immediately).
    Arena {
        name: Arc<str>,
        start_env_id: u32,
        type_val: Arc<Value>,
    },
    /// Type identity delegates to inner.type_val() — no own type_val field.
    ///
    /// Value annotated with runtime metadata (e.g. constructor annotation dict).
    /// Used by `make-annotated` and annotated unit constructors.
    /// `annotation` is a materialized `Value::Dict` of annotation key-value pairs.
    Annotated {
        inner: Box<Value>,
        annotation: Box<Value>,
    },
    /// Type-checker context handle — wraps `TypeContextData` for passing between tinct builtins.
    /// Created by `builtin-get-type-context`; consumed by `builtin-typecheck-doc`, `builtin-resolve`, etc.
    TypeContext {
        ctx: std::sync::Arc<std::sync::Mutex<crate::eval::TypeContextData>>,
        type_val: Arc<Value>,
    },
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
        type_val: Arc<Value>,
    },
}

impl Clone for Value {
    fn clone(&self) -> Self {
        match self {
            Value::Int { n, type_val } => Value::Int {
                n: *n,
                type_val: Arc::clone(type_val),
            },
            Value::U64 { n, type_val } => Value::U64 {
                n: *n,
                type_val: Arc::clone(type_val),
            },
            Value::Float { n, type_val } => Value::Float {
                n: *n,
                type_val: Arc::clone(type_val),
            },
            Value::String {
                source,
                start,
                end,
                type_val,
            } => Value::String {
                source: Arc::clone(source),
                start: *start,
                end: *end,
                type_val: Arc::clone(type_val),
            },
            Value::Dict { entries, type_val } => Value::Dict {
                entries: entries.clone(),
                type_val: Arc::clone(type_val),
            },
            Value::Builder(b) => Value::Builder(Arc::clone(b)),
            Value::Function {
                params,
                body,
                closure_env,
                annotation,
                type_val,
            } => Value::Function {
                params: Arc::clone(params),
                body: Arc::clone(body),
                closure_env: std::sync::Arc::clone(closure_env),
                annotation: annotation.clone(),
                type_val: Arc::clone(type_val),
            },
            Value::Builtin { def, type_val } => Value::Builtin {
                def: *def,
                type_val: Arc::clone(type_val),
            },
            Value::Proxy { handler, type_val } => Value::Proxy {
                handler: std::sync::Arc::clone(handler),
                type_val: Arc::clone(type_val),
            },
            Value::DirCap {
                dir,
                perms,
                type_val,
            } => Value::DirCap {
                // SAFETY: DirCap values are always created from valid OS file descriptors
                // (main.rs and builtins_io.rs construction sites). try_clone() can fail with
                // EMFILE (too many open files) or EBADF (invalid descriptor) only if the
                // descriptor is closed or the fd table is exhausted. DirCap values are assumed
                // to remain valid for their Arc lifetime — the descriptor is never explicitly
                // closed while a DirCap holding it is alive.
                dir: dir.try_clone().expect("DirCap try_clone"),
                perms: perms.clone(),
                type_val: Arc::clone(type_val),
            },
            Value::NetCap { entries, type_val } => Value::NetCap {
                entries: Arc::clone(entries),
                type_val: Arc::clone(type_val),
            },
            Value::File { inner, type_val } => Value::File {
                inner: Arc::clone(inner),
                type_val: Arc::clone(type_val),
            },
            Value::RevocableDirCap {
                inner,
                perms,
                revoked,
                type_val,
            } => Value::RevocableDirCap {
                inner: inner.try_clone().expect("RevocableDirCap try_clone"),
                perms: perms.clone(),
                revoked: Arc::clone(revoked),
                type_val: Arc::clone(type_val),
            },
            Value::Variant {
                type_val,
                ctor,
                payload,
            } => Value::Variant {
                type_val: Arc::clone(type_val),
                ctor: Arc::clone(ctor),
                payload: payload.as_ref().map(std::sync::Arc::clone),
            },
            Value::Decimal { n, type_val } => Value::Decimal {
                n: *n,
                type_val: Arc::clone(type_val),
            },
            Value::BigInt { n, type_val } => Value::BigInt {
                n: n.clone(),
                type_val: Arc::clone(type_val),
            },
            Value::Bytes {
                source,
                start,
                end,
                type_val,
            } => Value::Bytes {
                source: Arc::clone(source),
                start: *start,
                end: *end,
                type_val: Arc::clone(type_val),
            },
            Value::Uri {
                scheme,
                uri,
                type_val,
            } => Value::Uri {
                scheme: scheme.clone(),
                uri: uri.clone(),
                type_val: Arc::clone(type_val),
            },
            Value::Timestamp { ts, type_val } => Value::Timestamp {
                ts: ts.clone(),
                type_val: Arc::clone(type_val),
            },
            Value::Duration { nanos, type_val } => Value::Duration {
                nanos: *nanos,
                type_val: Arc::clone(type_val),
            },
            Value::ClockCap { inner, type_val } => Value::ClockCap {
                inner: Arc::clone(inner),
                type_val: Arc::clone(type_val),
            },
            Value::Timezone { tz, type_val } => Value::Timezone {
                tz: Arc::clone(tz),
                type_val: Arc::clone(type_val),
            },
            Value::QuicSession { conn, type_val } => Value::QuicSession {
                conn: Arc::clone(conn),
                type_val: Arc::clone(type_val),
            },
            Value::Http2Session {
                client,
                base_url,
                type_val,
            } => Value::Http2Session {
                client: Arc::clone(client),
                base_url: base_url.clone(),
                type_val: Arc::clone(type_val),
            },
            Value::Http3Session { session, type_val } => Value::Http3Session {
                session: Arc::clone(session),
                type_val: Arc::clone(type_val),
            },
            Value::QuicDatagramHandle { conn, type_val } => Value::QuicDatagramHandle {
                conn: Arc::clone(conn),
                type_val: Arc::clone(type_val),
            },
            Value::Program {
                program,
                resolutions,
                type_val,
            } => Value::Program {
                program: std::sync::Arc::clone(program),
                resolutions: Arc::clone(resolutions),
                type_val: Arc::clone(type_val),
            },
            Value::Document {
                doc,
                resolver_frames,
                type_val,
            } => Value::Document {
                doc: Arc::clone(doc),
                resolver_frames: Arc::clone(resolver_frames),
                type_val: Arc::clone(type_val),
            },
            Value::Expression { node, type_val } => Value::Expression {
                node: Arc::clone(node),
                type_val: Arc::clone(type_val),
            },
            Value::Task { state, type_val } => Value::Task {
                state: Arc::clone(state),
                type_val: Arc::clone(type_val),
            },
            Value::Channel { inner, type_val } => Value::Channel {
                inner: Arc::clone(inner),
                type_val: Arc::clone(type_val),
            },
            Value::BroadcastChannel { inner, type_val } => Value::BroadcastChannel {
                inner: Arc::clone(inner),
                type_val: Arc::clone(type_val),
            },
            Value::OneshotSender { inner, type_val } => Value::OneshotSender {
                inner: Arc::clone(inner),
                type_val: Arc::clone(type_val),
            },
            Value::OneshotReceiver { inner, type_val } => Value::OneshotReceiver {
                inner: Arc::clone(inner),
                type_val: Arc::clone(type_val),
            },
            Value::Context { token, type_val } => Value::Context {
                token: token.clone(),
                type_val: Arc::clone(type_val),
            },
            Value::ReactiveCell { inner, type_val } => Value::ReactiveCell {
                inner: Arc::clone(inner),
                type_val: Arc::clone(type_val),
            },
            Value::Arena {
                name,
                start_env_id,
                type_val,
            } => Value::Arena {
                name: Arc::clone(name),
                start_env_id: *start_env_id,
                type_val: Arc::clone(type_val),
            },
            Value::Annotated { inner, annotation } => Value::Annotated {
                inner: inner.clone(),
                annotation: annotation.clone(),
            },
            Value::TypeContext { ctx, type_val } => Value::TypeContext {
                ctx: Arc::clone(ctx),
                type_val: Arc::clone(type_val),
            },
            Value::CoreDocument {
                entries,
                span,
                type_val,
            } => Value::CoreDocument {
                entries: std::sync::Arc::clone(entries),
                span: span.clone(),
                type_val: Arc::clone(type_val),
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
        type_val: unknown_type_val(),
    }
}

/// Helper function to construct a `Value::Bytes` from a byte slice.
pub fn bytes_val(data: &[u8]) -> Value {
    Value::Bytes {
        source: Arc::from(data),
        start: 0,
        end: data.len(),
        type_val: unknown_type_val(),
    }
}

impl Value {
    /// Returns a human-readable type name for error messages and diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int { .. } => "Int",
            Value::U64 { .. } => "Int",
            Value::Float { .. } => "Float",
            Value::String { .. } => "String",
            Value::Dict { .. } => "Dict",
            Value::Builder(_) => "Builder",
            Value::Function { .. } => "Function",
            Value::Builtin { .. } => "Builtin",
            Value::Proxy { .. } => "Proxy",
            Value::DirCap { .. } => "DirCap",
            Value::NetCap { .. } => "NetCap",
            Value::File { .. } => "File",
            Value::RevocableDirCap { .. } => "DirCap",
            Value::Variant { .. } => "Variant",
            Value::Decimal { .. } => "Decimal",
            Value::BigInt { .. } => "BigInt",
            Value::Bytes { .. } => "Bytes",
            Value::Uri { .. } => "Uri",
            Value::Timestamp { .. } => "Timestamp",
            Value::Duration { .. } => "Duration",
            Value::ClockCap { .. } => "ClockCap",
            Value::Timezone { .. } => "Timezone",
            Value::QuicSession { .. } => "QuicSession",
            Value::Http2Session { .. } => "Http2Session",
            Value::Http3Session { .. } => "Http3Session",
            Value::QuicDatagramHandle { .. } => "QuicDatagramHandle",
            Value::Program { .. } => "Program",
            Value::Document { .. } => "Document",
            Value::Expression { .. } => "Expression",
            Value::Task { .. } => "Task",
            Value::Channel { .. } => "Channel",
            Value::BroadcastChannel { .. } => "BroadcastChannel",
            Value::OneshotSender { .. } => "OneshotSender",
            Value::OneshotReceiver { .. } => "OneshotReceiver",
            Value::Context { .. } => "Context",
            Value::ReactiveCell { .. } => "ReactiveCell",
            Value::Arena { .. } => "Arena",
            Value::Annotated { inner, .. } => inner.type_name(),
            Value::TypeContext { .. } => "TypeContext",
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
            Value::Document { .. } => Some("Document"),
            Value::TypeContext { .. } => Some("TypeContext"),
            // Both DirCap variants map to the declared "DirCap" type.
            Value::DirCap { .. } | Value::RevocableDirCap { .. } => Some("DirCap"),
            Value::NetCap { .. } => Some("NetCap"),
            Value::File { .. } => Some("File"),
            // type_name() returns "Builder" but the declared TyCon is "BuilderHandle".
            Value::Builder(_) => Some("BuilderHandle"),
            Value::Task { .. } => Some("Task"),
            Value::Channel { .. } => Some("Channel"),
            Value::Context { .. } => Some("Context"),
            Value::ReactiveCell { .. } => Some("ReactiveCell"),
            Value::ClockCap { .. } => Some("ClockCap"),
            Value::Timezone { .. } => Some("Timezone"),
            Value::Decimal { .. } => Some("Decimal"),
            Value::BigInt { .. } => Some("BigInt"),
            Value::QuicSession { .. } => Some("QuicSession"),
            Value::QuicDatagramHandle { .. } => Some("QuicDatagramHandle"),
            Value::Http2Session { .. } => Some("Http2Session"),
            Value::Http3Session { .. } => Some("Http3Session"),
            // All other values (Int, String, Float, Dict, Function,
            // Builtin, Variant, Bytes, Uri, Proxy, Annotated, etc.) are handled through
            // structural type checking or TyConDef constructor matching.
            _ => None,
        }
    }

    /// Extract a string slice from a `Value::String`, or `None` if not a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String {
                source, start, end, ..
            } => Some(&source[*start..*end]),
            _ => None,
        }
    }

    /// Extract a byte slice from a `Value::Bytes`, or `None` if not bytes.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes {
                source, start, end, ..
            } => Some(&source[*start..*end]),
            _ => None,
        }
    }

    /// Uniform accessor for a value's runtime type identity.
    /// Returns the type_val field for all variants that carry it.
    /// Builder returns the unknown sentinel; Annotated delegates to inner.
    pub fn type_val(&self) -> &Arc<Value> {
        match self {
            Value::Int { type_val, .. } => type_val,
            Value::U64 { type_val, .. } => type_val,
            Value::Float { type_val, .. } => type_val,
            Value::String { type_val, .. } => type_val,
            Value::Bytes { type_val, .. } => type_val,
            Value::Dict { type_val, .. } => type_val,
            Value::Function { type_val, .. } => type_val,
            Value::Builtin { type_val, .. } => type_val,
            Value::Proxy { type_val, .. } => type_val,
            Value::Variant { type_val, .. } => type_val,
            Value::Decimal { type_val, .. } => type_val,
            Value::BigInt { type_val, .. } => type_val,
            Value::Duration { type_val, .. } => type_val,
            Value::Uri { type_val, .. } => type_val,
            Value::Timestamp { type_val, .. } => type_val,
            Value::Timezone { type_val, .. } => type_val,
            Value::ClockCap { type_val, .. } => type_val,
            Value::DirCap { type_val, .. } => type_val,
            Value::NetCap { type_val, .. } => type_val,
            Value::File { type_val, .. } => type_val,
            Value::RevocableDirCap { type_val, .. } => type_val,
            Value::QuicSession { type_val, .. } => type_val,
            Value::Http2Session { type_val, .. } => type_val,
            Value::Http3Session { type_val, .. } => type_val,
            Value::QuicDatagramHandle { type_val, .. } => type_val,
            Value::Task { type_val, .. } => type_val,
            Value::Channel { type_val, .. } => type_val,
            Value::BroadcastChannel { type_val, .. } => type_val,
            Value::OneshotSender { type_val, .. } => type_val,
            Value::OneshotReceiver { type_val, .. } => type_val,
            Value::Context { type_val, .. } => type_val,
            Value::ReactiveCell { type_val, .. } => type_val,
            Value::Arena { type_val, .. } => type_val,
            Value::TypeContext { type_val, .. } => type_val,
            Value::Program { type_val, .. } => type_val,
            Value::Document { type_val, .. } => type_val,
            Value::Expression { type_val, .. } => type_val,
            Value::CoreDocument { type_val, .. } => type_val,
            // Exceptions:
            Value::Builder(_) => unknown_type_val_ref(),
            Value::Annotated { inner, .. } => inner.type_val(),
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int { n, .. } => f.debug_tuple("Int").field(n).finish(),
            Value::U64 { n, .. } => f.debug_tuple("U64").field(n).finish(),
            Value::Float { n, .. } => f.debug_tuple("Float").field(n).finish(),
            Value::String {
                source, start, end, ..
            } => {
                let s = &source[*start..*end];
                f.debug_tuple("String").field(&s).finish()
            }
            Value::Dict { entries, .. } => {
                let keys: Vec<&HashableValue> = entries.keys().collect();
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
            Value::Builtin { def, .. } => write!(f, "Builtin({})", def.name),
            Value::Proxy { .. } => write!(f, "Proxy"),
            Value::DirCap { .. } => write!(f, "DirCap"),
            Value::NetCap { entries, .. } => write!(f, "NetCap({} entries)", entries.len()),
            Value::File { .. } => write!(f, "File"),
            Value::RevocableDirCap { revoked, .. } => {
                if revoked.load(Ordering::Acquire) {
                    write!(f, "DirCap(revoked)")
                } else {
                    write!(f, "DirCap(revocable)")
                }
            }
            Value::Variant { ctor, payload, .. } => {
                if payload.is_some() {
                    write!(f, "Variant({}, <payload>)", ctor)
                } else {
                    write!(f, "Variant({})", ctor)
                }
            }
            Value::Decimal { n, .. } => write!(f, "Decimal({n})"),
            Value::BigInt { n, .. } => write!(f, "BigInt({n})"),
            Value::Bytes {
                source, start, end, ..
            } => {
                let bytes = &source[*start..*end];
                write!(f, "Bytes({} bytes)", bytes.len())
            }
            Value::Uri { scheme, uri, .. } => write!(f, "Uri({scheme}:{uri})"),
            Value::Timestamp { ts, .. } => write!(f, "Timestamp({} ns)", ts.as_nanosecond()),
            Value::Duration { nanos, .. } => write!(f, "Duration({nanos} ns)"),
            Value::ClockCap { inner, .. } => match inner.as_ref() {
                ClockCapInner::Real => write!(f, "ClockCap(Real)"),
                ClockCapInner::Fixed(nanos) => write!(f, "ClockCap(Fixed({nanos} ns))"),
            },
            Value::Timezone { .. } => write!(f, "Timezone"),
            Value::QuicSession { .. } => write!(f, "QuicSession"),
            Value::Http2Session { base_url, .. } => write!(f, "Http2Session({base_url})"),
            Value::Http3Session { .. } => write!(f, "Http3Session"),
            Value::QuicDatagramHandle { .. } => write!(f, "QuicDatagramHandle"),
            Value::Program { .. } => write!(f, "Program(...)"),
            Value::Document { .. } => write!(f, "Document(...)"),
            Value::Expression { node, .. } => write!(
                f,
                "Expression({})",
                crate::surface_fields::surface_expr_tag(&node.expr)
            ),
            Value::Task { .. } => write!(f, "Task"),
            Value::Channel { .. } => write!(f, "Channel"),
            Value::Context { .. } => write!(f, "Context"),
            Value::ReactiveCell { .. } => write!(f, "ReactiveCell"),
            Value::BroadcastChannel { .. } => write!(f, "BroadcastChannel"),
            Value::OneshotSender { .. } => write!(f, "OneshotSender"),
            Value::OneshotReceiver { .. } => write!(f, "OneshotReceiver"),
            Value::Arena {
                name, start_env_id, ..
            } => write!(f, "Arena({name}@{start_env_id})"),
            Value::Annotated { inner, .. } => write!(f, "Annotated({inner:?})"),
            Value::TypeContext { .. } => write!(f, "TypeContext"),
            Value::CoreDocument { entries, .. } => {
                write!(f, "CoreDocument({} entries)", entries.len())
            }
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int { n, .. } => write!(f, "{n}"),
            Value::U64 { n, .. } => write!(f, "{n}"),
            Value::Float { n, .. } => write!(f, "{n}"),
            Value::String {
                source, start, end, ..
            } => {
                let s = &source[*start..*end];
                write!(f, "{s:?}")
            }
            Value::Dict { entries, .. } => {
                write!(f, "[")?;
                for (i, (key, _)) in entries.iter().enumerate() {
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
            Value::Builtin { def, .. } => write!(f, "<builtin {}>", def.name),
            Value::Proxy { .. } => write!(f, "<proxy>"),
            Value::DirCap { .. } => write!(f, "<DirCap>"),
            Value::NetCap { .. } => write!(f, "<NetCap>"),
            Value::File { .. } => write!(f, "<File>"),
            Value::RevocableDirCap { revoked, .. } => {
                if revoked.load(Ordering::Acquire) {
                    write!(f, "<DirCap (revoked)>")
                } else {
                    write!(f, "<DirCap (revocable)>")
                }
            }
            Value::Variant { ctor, payload, .. } => {
                if payload.is_some() {
                    write!(f, "{}(<payload>)", ctor)
                } else {
                    write!(f, "{}", ctor)
                }
            }
            Value::Decimal { n, .. } => write!(f, "{n}"),
            Value::BigInt { n, .. } => write!(f, "{n}"),
            Value::Bytes {
                source, start, end, ..
            } => {
                let bytes = &source[*start..*end];
                write!(f, "<bytes:{} bytes>", bytes.len())
            }
            Value::Uri { uri, .. } => write!(f, "{uri}"),
            Value::Timestamp { ts, .. } => write!(f, "{ts}"),
            Value::Duration { nanos, .. } => {
                write!(f, "{nanos}ns")
            }
            Value::ClockCap { .. } => write!(f, "<ClockCap>"),
            Value::Timezone { .. } => write!(f, "<Timezone>"),
            Value::QuicSession { .. } => write!(f, "<QuicSession>"),
            Value::Http2Session { base_url, .. } => write!(f, "<Http2Session {base_url}>"),
            Value::Http3Session { .. } => write!(f, "<Http3Session>"),
            Value::QuicDatagramHandle { .. } => write!(f, "<QuicDatagramHandle>"),
            Value::Program { .. } => write!(f, "<program>"),
            Value::Document { .. } => write!(f, "<document>"),
            Value::Expression { node, .. } => write!(
                f,
                "<expression:{}>",
                crate::surface_fields::surface_expr_tag(&node.expr)
            ),
            Value::Task { .. } => write!(f, "<task>"),
            Value::Channel { .. } => write!(f, "<channel>"),
            Value::Context { .. } => write!(f, "<context>"),
            Value::ReactiveCell { .. } => write!(f, "<reactive-cell>"),
            Value::BroadcastChannel { .. } => write!(f, "<broadcast-channel>"),
            Value::OneshotSender { .. } => write!(f, "<oneshot-sender>"),
            Value::OneshotReceiver { .. } => write!(f, "<oneshot-receiver>"),
            Value::Arena {
                name, start_env_id, ..
            } => write!(f, "<arena:{name}@{start_env_id}>"),
            Value::Annotated { inner, .. } => fmt::Display::fmt(inner, f),
            Value::TypeContext { .. } => write!(f, "<TypeContext>"),
            Value::CoreDocument { entries, .. } => {
                write!(f, "<core-document:{} entries>", entries.len())
            }
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int { n: a, .. }, Value::Int { n: b, .. }) => a == b,
            (Value::U64 { n: a, .. }, Value::U64 { n: b, .. }) => a == b,
            (Value::Float { n: a, .. }, Value::Float { n: b, .. }) => a == b,
            (
                Value::String {
                    source: src_a,
                    start: start_a,
                    end: end_a,
                    ..
                },
                Value::String {
                    source: src_b,
                    start: start_b,
                    end: end_b,
                    ..
                },
            ) => src_a[*start_a..*end_a] == src_b[*start_b..*end_b],
            (Value::Decimal { n: a, .. }, Value::Decimal { n: b, .. }) => a == b,
            (Value::BigInt { n: a, .. }, Value::BigInt { n: b, .. }) => a == b,
            (
                Value::Bytes {
                    source: src_a,
                    start: start_a,
                    end: end_a,
                    ..
                },
                Value::Bytes {
                    source: src_b,
                    start: start_b,
                    end: end_b,
                    ..
                },
            ) => src_a[*start_a..*end_a] == src_b[*start_b..*end_b],
            (
                Value::Uri {
                    scheme: scheme_a,
                    uri: uri_a,
                    ..
                },
                Value::Uri {
                    scheme: scheme_b,
                    uri: uri_b,
                    ..
                },
            ) => scheme_a == scheme_b && uri_a == uri_b,
            (Value::Timestamp { ts: a, .. }, Value::Timestamp { ts: b, .. }) => a == b,
            (Value::Duration { nanos: a, .. }, Value::Duration { nanos: b, .. }) => a == b,
            (Value::ClockCap { inner: a, .. }, Value::ClockCap { inner: b, .. }) => a == b,
            (Value::QuicSession { conn: a, .. }, Value::QuicSession { conn: b, .. }) => {
                Arc::ptr_eq(a, b)
            }
            (Value::Http2Session { client: a, .. }, Value::Http2Session { client: b, .. }) => {
                Arc::ptr_eq(a, b)
            }
            (Value::Http3Session { session: a, .. }, Value::Http3Session { session: b, .. }) => {
                Arc::ptr_eq(a, b)
            }
            (
                Value::QuicDatagramHandle { conn: a, .. },
                Value::QuicDatagramHandle { conn: b, .. },
            ) => Arc::ptr_eq(a, b),
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
        expr: std::sync::Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
        frame: std::sync::Arc<EvalFrame>,
        ctx: Arc<crate::eval::EvalContext>,
    },
    /// Deferred builtin call (was PendingBuiltin).
    BuiltinCall {
        def: BuiltinDef,
        args: Vec<std::sync::Arc<Thunk>>,
        named: Option<indexmap::IndexMap<String, std::sync::Arc<Thunk>>>,
        call_span: Span,
        /// Caller environment identity; used by `builtin-current-env` for scope introspection.
        caller_env_id: u32,
        ctx: Arc<crate::eval::EvalContext>,
    },
    /// Deferred function call (was PendingCall).
    FnCall {
        func: std::sync::Arc<Thunk>,
        args: Vec<std::sync::Arc<Thunk>>,
        named: Option<Box<indexmap::IndexMap<String, std::sync::Arc<Thunk>>>>,
        call_span: Span,
        /// Caller environment identity; used by `builtin-current-env` for scope introspection.
        caller_env_id: u32,
        ctx: Arc<crate::eval::EvalContext>,
        /// Original CoreExpr::Call node for PendingBuiltin lazy-arg re-dispatch.
        original_call: Arc<Spanned<CoreExpr>>,
    },
    /// Type guard wrapping an inner thunk (was Guarded).
    Guarded {
        inner: std::sync::Arc<Thunk>,
        /// TypeValue (Arc<Value>) representing the expected type for this guard.
        /// Uses the TypeValue representation (TypeValue.Repr, TypeValue.Record, etc.)
        /// rather than the deleted Type enum. Set by Thunk::guarded.
        expected: std::sync::Arc<Value>,
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
        inner: std::sync::Arc<Thunk>,
        annotation: Box<Value>,
        ctx: Arc<crate::eval::EvalContext>,
    },
}

impl UnevaluatedState {
    pub fn initial_env_id(&self) -> u32 {
        match self {
            UnevaluatedState::CoreExpr { .. } => 0,
            UnevaluatedState::BuiltinCall { caller_env_id, .. } => *caller_env_id,
            UnevaluatedState::FnCall { caller_env_id, .. } => *caller_env_id,
            UnevaluatedState::AstField { .. } => 0,
            UnevaluatedState::Guarded { .. } => 0,
            UnevaluatedState::AnnotatedWrap { .. } => 0,
        }
    }
}

/// Groups the call-site information for a user-function invocation into a single value.
///
/// Passed to `Thunk::fn_call` to reduce the argument count below Clippy's limit.
pub struct FnCallSpec {
    /// The span of the call expression (for error messages).
    pub call_span: Span,
    /// The caller's FlatEnv id (for `builtin-current-env` and future scope wiring).
    pub caller_env_id: u32,
    /// The evaluation context.
    pub ctx: Arc<crate::eval::EvalContext>,
    /// The original call expression (for error messages).
    pub original_call: Arc<Spanned<CoreExpr>>,
}

/// New thunk structure for async evaluation (Sprint 2B).
/// Two-field pair for thunk state:
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
    /// Lazily initialized: only allocated when `settled()` is first awaited.
    pub notify: std::sync::OnceLock<Arc<tokio::sync::Notify>>,
}

/// Lazy evaluation cell: wraps an unevaluated expression, a pending builtin call,
/// or a materialized value with memoization (evaluate-at-most-once semantics).
pub struct Thunk {
    pub(crate) inner: ThunkInner,
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
                notify: std::sync::OnceLock::new(),
            },
            span,
        }
    }

    /// Create an unevaluated thunk from a CoreExpr body (no Expr round-trip).
    pub fn core_expr(
        expr: std::sync::Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
        frame: std::sync::Arc<EvalFrame>,
        ctx: Arc<crate::eval::EvalContext>,
        span: Span,
    ) -> Self {
        Self {
            inner: ThunkInner {
                unevaluated: Mutex::new((
                    Some(UnevaluatedState::CoreExpr { expr, frame, ctx }),
                    None,
                )),
                result: tokio::sync::OnceCell::new(),
                notify: std::sync::OnceLock::new(),
            },
            span,
        }
    }

    pub fn value(value: Value, span: Span) -> Self {
        let inner = ThunkInner {
            unevaluated: Mutex::new((None, None)),
            result: tokio::sync::OnceCell::new(),
            notify: std::sync::OnceLock::new(),
        };
        // OnceCell::set returns Err(value) if already set — impossible here because
        // ThunkInner was just created above with a fresh OnceCell.
        if inner.result.set(Ok(value)).is_err() {
            // This branch is statically unreachable: the OnceCell was just constructed
            // and no other thread can hold a reference to `inner` yet.
            panic!("value thunk cell freshly created: set cannot fail");
        }
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
                notify: std::sync::OnceLock::new(),
            },
            span,
        }
    }

    /// Create a deferred Value::Annotated thunk (T-1621).
    ///
    /// When forced, materializes `inner` and produces
    /// `Value::Annotated { inner: forced_inner, annotation }`.
    pub fn annotated_wrap(
        inner: std::sync::Arc<Thunk>,
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
                notify: std::sync::OnceLock::new(),
            },
            span,
        }
    }

    pub fn builtin_call(
        def: BuiltinDef,
        args: Vec<std::sync::Arc<Thunk>>,
        named: Option<indexmap::IndexMap<String, std::sync::Arc<Thunk>>>,
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
                notify: std::sync::OnceLock::new(),
            },
            span,
        }
    }

    pub fn fn_call(
        func: std::sync::Arc<Thunk>,
        args: Vec<std::sync::Arc<Thunk>>,
        named: indexmap::IndexMap<String, std::sync::Arc<Thunk>>,
        span: Span,
        spec: FnCallSpec,
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
                        call_span: spec.call_span,
                        caller_env_id: spec.caller_env_id,
                        ctx: spec.ctx,
                        original_call: spec.original_call,
                    }),
                    None,
                )),
                result: tokio::sync::OnceCell::new(),
                notify: std::sync::OnceLock::new(),
            },
            span,
        }
    }

    pub fn guarded(
        inner: std::sync::Arc<Thunk>,
        expected: std::sync::Arc<Value>,
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
                notify: std::sync::OnceLock::new(),
            },
            span: guard_span.with_name(Arc::from("type guard")),
        }
    }

    /// Return the source span where this thunk was created.
    pub fn definition_span(&self) -> Span {
        self.span.clone()
    }

    /// Set the terminal result for this thunk. Write-once via OnceCell:
    /// the first settle() wins, subsequent calls are no-ops.
    ///
    /// Failed thunks cannot accumulate materialization spans from later call sites.
    /// The first error's span is preserved; this is the accepted design — the first
    /// materialization span is closest to the definition site and is the most
    /// informative for debugging.
    ///
    /// Once a thunk is settled (Ok or Err), it cannot transition to any other state.
    /// A materialized thunk cannot become Failed, even at high continuation depth.
    /// The depth limit is enforced at the continuation stack
    /// level (MAX_CONTINUATION_STACK in eval_materialize.rs) before any thunk is
    /// settled, which is the correct enforcement point.
    pub fn settle(&self, result: Result<Value, Arc<EvalError>>) {
        // OnceCell::set returns Err(value) if already set — designed concurrent write pattern.
        // Multiple async tasks may race to settle the same thunk (e.g. racing evaluators on
        // a shared thunk). The first settle wins; the losing settle's result is intentionally
        // discarded. This is correct: all racing evaluators produce the same value for a pure
        // thunk — the result is deterministic by referential transparency. The Err payload
        // (the rejected result) is not an application error; it is the concurrent-duplicate
        // that OnceCell prevents from overwriting the winner.
        match self.inner.result.set(result) {
            Ok(()) => {}
            Err(_duplicate) => {
                // Concurrent duplicate: a racing task already settled this thunk.
                // The first settler wins by OnceCell semantics; this outcome is discarded
                // by design — all racing evaluators produce the same value for a pure thunk.
            }
        }
        {
            let mut guard = self.inner.unevaluated.lock().unwrap();
            guard.1 = None;
        }
        if let Some(n) = self.inner.notify.get() {
            n.notify_waiters();
        }
    }

    pub async fn settled(&self) {
        loop {
            let notified = self
                .inner
                .notify
                .get_or_init(|| Arc::new(tokio::sync::Notify::new()))
                .notified();
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
                frame,
                ctx: _,
            } => UnevaluatedState::CoreExpr {
                expr,
                frame,
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
                notify: std::sync::OnceLock::new(),
            },
            span: self.span.clone(),
        }))
    }

    /// Inspect the settled result without discarding the error case.
    ///
    /// Returns:
    /// - `None` — thunk is not yet settled (still being evaluated)
    /// - `Some(Ok(&value))` — thunk settled successfully; value is available
    /// - `Some(Err(&arc_error))` — thunk settled with an evaluation error
    ///
    /// Use this when the caller needs to distinguish all three states.
    pub fn peek_result(&self) -> Option<Result<&Value, &Arc<EvalError>>> {
        self.inner.result.get().map(|r| r.as_ref())
    }

    /// Get the materialized value, propagating any evaluation error.
    ///
    /// For use at call sites where the thunk is guaranteed to be settled
    /// (pre-materialized by `pos_strictness` or `force_count`).
    ///
    /// - `Ok(&value)` — thunk settled successfully
    /// - `Err(Box<EvalError>)` — thunk settled with an evaluation error (propagated)
    /// - Panics if the thunk is not yet settled (invariant violation)
    pub fn require_value(&self) -> crate::error::EvalResult<&Value> {
        match self.peek_result() {
            Some(Ok(v)) => Ok(v),
            Some(Err(e)) => Err(Box::new((**e).clone())),
            None => {
                panic!("require_value called on unsettled thunk — expected pre-materialized thunk")
            }
        }
    }

    /// Borrow the cached error without cloning. Returns `None` if the thunk
    /// is not yet settled or settled with a value.
    pub fn try_get_error(&self) -> Option<&Arc<EvalError>> {
        self.inner.result.get()?.as_ref().err()
    }

    /// Check whether this thunk has successfully materialized.
    pub fn is_materialized(&self) -> bool {
        matches!(self.inner.result.get(), Some(Ok(_)))
    }

    /// Check whether this thunk has reached a terminal state (materialized or failed).
    pub fn is_settled(&self) -> bool {
        self.inner.result.get().is_some()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{CoreExpr, Spanned};
    use crate::test_util::test_span;

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        crate::eval::EvalContext::new()
    }

    #[test]
    fn test_state_of_unevaluated() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);
        let expr = Arc::new(Spanned::new(CoreExpr::Int(42), span.clone()));
        let thunk = Thunk::core_expr(expr, EvalFrame::empty(), ctx, span);

        assert!(!thunk.is_settled(), "Expected unevaluated (not settled)");
        assert!(
            thunk.inner.unevaluated.lock().unwrap().0.is_some(),
            "Expected unevaluated state to be Some"
        );
    }

    #[test]
    fn test_state_of_materialized() {
        let span = test_span(1, 1, 1, 10);
        let thunk = Thunk::value(
            Value::Int {
                n: 42,
                type_val: unknown_type_val(),
            },
            span,
        );

        assert_eq!(
            match thunk.peek_result() {
                Some(Ok(v)) => Some(v),
                Some(Err(e)) => panic!("unexpected error in test thunk: {e}"),
                None => None,
            },
            Some(&Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }),
            "Expected materialized Int(42)"
        );
    }

    #[test]
    fn test_settle_ok() {
        let span = test_span(1, 1, 1, 10);
        let thunk = Thunk::placeholder(span);

        thunk.settle(Ok(Value::Int {
            n: 1,
            type_val: unknown_type_val(),
        }));

        assert_eq!(
            match thunk.peek_result() {
                Some(Ok(v)) => Some(v),
                Some(Err(e)) => panic!("unexpected error in test thunk: {e}"),
                None => None,
            },
            Some(&Value::Int {
                n: 1,
                type_val: unknown_type_val()
            }),
            "Expected materialized Int(1)"
        );
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

        let e = thunk.try_get_error().expect("Expected Failed state");
        assert_eq!(Arc::as_ptr(e), Arc::as_ptr(&error));
    }

    #[test]
    fn test_settle_idempotent() {
        let span = test_span(1, 1, 1, 10);
        let thunk = Thunk::placeholder(span);

        thunk.settle(Ok(Value::Int {
            n: 1,
            type_val: unknown_type_val(),
        }));
        // Second settle should be no-op (OnceCell ignores duplicate set)
        thunk.settle(Ok(Value::Int {
            n: 999,
            type_val: unknown_type_val(),
        }));

        assert_eq!(
            match thunk.peek_result() {
                Some(Ok(v)) => Some(v),
                Some(Err(e)) => panic!("unexpected error in test thunk: {e}"),
                None => None,
            },
            Some(&Value::Int {
                n: 1,
                type_val: unknown_type_val()
            }),
            "Expected materialized Int(1)"
        );
    }

    // First-error-wins: failed thunk cannot accumulate spans — the first error's span is kept.
    #[test]
    fn test_b424_failed_thunk_first_error_wins() {
        let span1 = test_span(1, 1, 1, 10);
        let span2 = test_span(5, 1, 5, 10);
        let thunk = Thunk::placeholder(span1.clone());

        let error1 = Arc::new(crate::error::EvalError::internal(
            "first error".to_string(),
            span1,
        ));
        let error2 = Arc::new(crate::error::EvalError::internal(
            "second error".to_string(),
            span2,
        ));

        thunk.settle(Err(Arc::clone(&error1)));
        // Second settle with different error is a no-op (OnceCell write-once).
        thunk.settle(Err(Arc::clone(&error2)));

        let e = thunk.try_get_error().expect("Expected Failed state");
        assert_eq!(
            Arc::as_ptr(e),
            Arc::as_ptr(&error1),
            "First error must win — OnceCell is write-once"
        );
    }

    // Settled thunk is write-once: a materialized thunk cannot transition to Failed.
    #[test]
    fn test_b425_materialized_thunk_cannot_become_failed() {
        let span = test_span(1, 1, 1, 10);
        let thunk = Thunk::placeholder(span.clone());

        // Settle with success first.
        thunk.settle(Ok(Value::Int {
            n: 42,
            type_val: unknown_type_val(),
        }));

        // Attempt to settle with error — must be a no-op.
        let error = Arc::new(crate::error::EvalError::internal(
            "should not replace".to_string(),
            span,
        ));
        thunk.settle(Err(Arc::clone(&error)));

        // Thunk should still be Ok(42).
        assert_eq!(
            match thunk.peek_result() {
                Some(Ok(v)) => Some(v),
                Some(Err(e)) => panic!("unexpected error in test thunk: {e}"),
                None => None,
            },
            Some(&Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }),
            "Materialized thunk must not transition to Failed — OnceCell write-once"
        );
        assert!(
            thunk.try_get_error().is_none(),
            "No error should be present after materialization"
        );
    }

    #[test]
    fn test_try_claim_transitions_to_inprogress() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);
        let expr = Arc::new(Spanned::new(CoreExpr::Int(42), span.clone()));
        let thunk = Thunk::core_expr(expr.clone(), EvalFrame::empty(), ctx.clone(), span);

        let state = thunk.try_claim();
        assert!(state.is_some(), "try_claim should succeed on unevaluated");

        // Verify the returned state is CoreExpr
        match state.unwrap() {
            UnevaluatedState::CoreExpr { expr: e, .. } => {
                assert_eq!(Arc::as_ptr(&e), Arc::as_ptr(&expr));
            }
            other => panic!("Expected CoreExpr state, got {:?}", other),
        }

        // Verify thunk is now InProgress (not settled, unevaluated is None)
        assert!(!thunk.is_settled(), "Expected not settled (InProgress)");
        assert!(
            thunk.inner.unevaluated.lock().unwrap().0.is_none(),
            "Expected unevaluated to be None (InProgress)"
        );
    }

    #[test]
    fn test_try_claim_returns_none_when_inprogress() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);
        let expr = Arc::new(Spanned::new(CoreExpr::Int(42), span.clone()));
        let thunk = Thunk::core_expr(expr, EvalFrame::empty(), ctx, span);

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
        let thunk = Thunk::core_expr(expr, EvalFrame::empty(), ctx.clone(), span);

        let state = thunk.try_claim().expect("try_claim should succeed");

        // Verify InProgress (not settled, unevaluated is None)
        assert!(!thunk.is_settled(), "Expected not settled (InProgress)");
        assert!(
            thunk.inner.unevaluated.lock().unwrap().0.is_none(),
            "Expected unevaluated to be None (InProgress)"
        );

        thunk.reset(state);

        // Verify restored to Unevaluated (not settled, unevaluated is Some)
        assert!(!thunk.is_settled(), "Expected not settled (Unevaluated)");
        assert!(
            thunk.inner.unevaluated.lock().unwrap().0.is_some(),
            "Expected unevaluated to be Some (Unevaluated)"
        );
    }

    #[test]
    fn test_peek_result_materialized() {
        let span = test_span(1, 1, 1, 10);
        let thunk = Thunk::value(
            Value::Int {
                n: 42,
                type_val: unknown_type_val(),
            },
            span,
        );

        assert_eq!(
            match thunk.peek_result() {
                Some(Ok(v)) => Some(v),
                Some(Err(e)) => panic!("unexpected error in test thunk: {e}"),
                None => None,
            },
            Some(&Value::Int {
                n: 42,
                type_val: unknown_type_val()
            }),
            "Expected Some(Ok(&Int(42)))"
        );
    }

    #[test]
    fn test_peek_result_settled_with_error_not_hidden() {
        // Verify that peek_result exposes the error — not None.
        let span = test_span(1, 1, 1, 10);
        let thunk = Thunk::placeholder(span.clone());
        let error = Arc::new(crate::error::EvalError::internal(
            "settle error".to_string(),
            span,
        ));
        thunk.settle(Err(Arc::clone(&error)));

        // peek_result must return Some(Err(...)) — not None.
        match thunk.peek_result() {
            Some(Err(e)) => assert_eq!(Arc::as_ptr(e), Arc::as_ptr(&error)),
            other => panic!("expected Some(Err(...)), got {:?}", other),
        }
    }

    #[test]
    fn test_try_get_error_convenience() {
        let span = test_span(1, 1, 1, 10);
        let thunk = Thunk::placeholder(span.clone());

        let error = Arc::new(crate::error::EvalError::internal(
            "test error".to_string(),
            span,
        ));
        thunk.settle(Err(Arc::clone(&error)));

        let cached = thunk.try_get_error();
        assert!(cached.is_some(), "try_get_error should return Some");
        assert_eq!(cached.unwrap().kind.to_string(), error.kind.to_string());
    }

    // =========================================================================
    // HashableValue property tests
    // =========================================================================

    use std::collections::hash_map::DefaultHasher;

    fn compute_hash(value: &HashableValue) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn hashable_value_eq_implies_hash_eq_int() {
        let a = HashableValue::Int(42);
        let b = HashableValue::Int(42);
        assert_eq!(a, b);
        assert_eq!(compute_hash(&a), compute_hash(&b));
    }

    #[test]
    fn hashable_value_eq_implies_hash_eq_str() {
        let a = HashableValue::Str("hello".into());
        let b = HashableValue::Str("hello".into());
        assert_eq!(a, b);
        assert_eq!(compute_hash(&a), compute_hash(&b));
    }

    #[test]
    fn hashable_value_cross_type_inequality_int_str() {
        let int_val = HashableValue::Int(0);
        let str_val = HashableValue::Str("0".into());
        assert_ne!(int_val, str_val);
        // Hashes should also differ (different discriminants)
        assert_ne!(compute_hash(&int_val), compute_hash(&str_val));

        let int_val = HashableValue::Int(1);
        let str_val = HashableValue::Str("1".into());
        assert_ne!(int_val, str_val);
        assert_ne!(compute_hash(&int_val), compute_hash(&str_val));
    }

    #[test]
    fn hashable_value_hash_stability() {
        let int_val = HashableValue::Int(42);
        let h1 = compute_hash(&int_val);
        let h2 = compute_hash(&int_val);
        assert_eq!(h1, h2, "Hash of Int(42) must be deterministic");

        let str_val = HashableValue::Str("key".into());
        let h1 = compute_hash(&str_val);
        let h2 = compute_hash(&str_val);
        assert_eq!(h1, h2, "Hash of Str(\"key\") must be deterministic");
    }

    #[test]
    fn hashable_value_distinct_values_have_distinct_hashes() {
        let a = HashableValue::Int(1);
        let b = HashableValue::Int(2);
        assert_ne!(a, b);
        assert_ne!(
            compute_hash(&a),
            compute_hash(&b),
            "Int(1) and Int(2) should have different hashes"
        );

        let a = HashableValue::Str("foo".into());
        let b = HashableValue::Str("bar".into());
        assert_ne!(a, b);
        assert_ne!(
            compute_hash(&a),
            compute_hash(&b),
            "Str(\"foo\") and Str(\"bar\") should have different hashes"
        );
    }

    #[test]
    fn hashable_value_dict_order_insensitive_eq() {
        // Dict equality is order-insensitive
        let d1 = HashableValue::Dict(vec![
            (HashableValue::Str("a".into()), HashableValue::Int(1)),
            (HashableValue::Str("b".into()), HashableValue::Int(2)),
        ]);
        let d2 = HashableValue::Dict(vec![
            (HashableValue::Str("b".into()), HashableValue::Int(2)),
            (HashableValue::Str("a".into()), HashableValue::Int(1)),
        ]);
        assert_eq!(d1, d2, "Dict equality must be order-insensitive");
        assert_eq!(
            compute_hash(&d1),
            compute_hash(&d2),
            "Dict hash must be order-insensitive"
        );
    }

    #[test]
    fn hashable_value_dict_different_values() {
        let d1 = HashableValue::Dict(vec![(
            HashableValue::Str("a".into()),
            HashableValue::Int(1),
        )]);
        let d2 = HashableValue::Dict(vec![(
            HashableValue::Str("a".into()),
            HashableValue::Int(2),
        )]);
        assert_ne!(d1, d2);
    }

    #[test]
    fn hashable_value_dict_different_lengths() {
        let d1 = HashableValue::Dict(vec![(
            HashableValue::Str("a".into()),
            HashableValue::Int(1),
        )]);
        let d2 = HashableValue::Dict(vec![
            (HashableValue::Str("a".into()), HashableValue::Int(1)),
            (HashableValue::Str("b".into()), HashableValue::Int(2)),
        ]);
        assert_ne!(d1, d2);
    }

    #[test]
    fn hashable_value_variant_eq_and_hash() {
        let v1 = HashableValue::Variant {
            tag: "Color.Red".into(),
            payload: None,
        };
        let v2 = HashableValue::Variant {
            tag: "Color.Red".into(),
            payload: None,
        };
        assert_eq!(v1, v2);
        assert_eq!(compute_hash(&v1), compute_hash(&v2));

        let v3 = HashableValue::Variant {
            tag: "Color.Blue".into(),
            payload: None,
        };
        assert_ne!(v1, v3);
    }

    #[test]
    fn hashable_value_variant_with_payload() {
        let v1 = HashableValue::Variant {
            tag: "Option.Some".into(),
            payload: Some(Box::new(HashableValue::Int(42))),
        };
        let v2 = HashableValue::Variant {
            tag: "Option.Some".into(),
            payload: Some(Box::new(HashableValue::Int(42))),
        };
        assert_eq!(v1, v2);
        assert_eq!(compute_hash(&v1), compute_hash(&v2));

        let v3 = HashableValue::Variant {
            tag: "Option.Some".into(),
            payload: Some(Box::new(HashableValue::Int(99))),
        };
        assert_ne!(v1, v3);
    }

    #[test]
    fn hashable_value_variant_cross_type() {
        let variant = HashableValue::Variant {
            tag: "Color.Red".into(),
            payload: None,
        };
        let str_val = HashableValue::Str("Color.Red".into());
        assert_ne!(variant, str_val);
    }

    #[test]
    fn hashable_value_dict_duplicate_keys_no_panic() {
        // HashableValue::Dict with duplicate keys should not panic on equality check.
        // Instead, it should return false (not equal to anything, even itself with dupes).
        let d_with_dupes = HashableValue::Dict(vec![
            (HashableValue::Str("a".into()), HashableValue::Int(1)),
            (HashableValue::Str("a".into()), HashableValue::Int(2)),
        ]);
        let d_normal = HashableValue::Dict(vec![(
            HashableValue::Str("a".into()),
            HashableValue::Int(1),
        )]);
        // Should not panic — returns false because d_with_dupes has duplicate keys
        assert_ne!(d_with_dupes, d_normal);
        // Self-comparison with dupes also returns false (duplicate key detected)
        assert_ne!(d_with_dupes, d_with_dupes.clone());
    }

    // =========================================================================
    // HashableValue::Float tests (D-9 / S-972: TotalF64 bitwise equality)
    // =========================================================================

    #[test]
    fn hashable_value_float_eq_implies_hash_eq() {
        // Same bit pattern → equal and same hash.
        let a = HashableValue::Float(3.14f64.to_bits());
        let b = HashableValue::Float(3.14f64.to_bits());
        assert_eq!(a, b);
        assert_eq!(compute_hash(&a), compute_hash(&b));
    }

    #[test]
    fn hashable_value_float_different_values_not_equal() {
        let a = HashableValue::Float(1.0f64.to_bits());
        let b = HashableValue::Float(2.0f64.to_bits());
        assert_ne!(a, b);
        assert_ne!(compute_hash(&a), compute_hash(&b));
    }

    #[test]
    fn hashable_value_float_nan_eq_same_bits() {
        // TotalF64: NaN == NaN when they share the same bit pattern.
        let nan_bits = f64::NAN.to_bits();
        let a = HashableValue::Float(nan_bits);
        let b = HashableValue::Float(nan_bits);
        assert_eq!(a, b, "NaN with same bit pattern must be equal as key");
        assert_eq!(compute_hash(&a), compute_hash(&b));
    }

    #[test]
    fn hashable_value_float_neg_zero_and_pos_zero_distinct() {
        // -0.0 and +0.0 have different bit patterns and are distinct keys.
        let pos_zero = HashableValue::Float(0.0f64.to_bits());
        let neg_zero = HashableValue::Float((-0.0f64).to_bits());
        assert_ne!(pos_zero, neg_zero, "-0.0 and +0.0 must be distinct keys");
    }

    #[test]
    fn hashable_value_float_cross_type_not_equal_to_int() {
        // Float(1.0) is not equal to Int(1) — different types.
        let float_one = HashableValue::Float(1.0f64.to_bits());
        let int_one = HashableValue::Int(1);
        assert_ne!(float_one, int_one);
        // Hashes differ because discriminants differ (Float=5, Int=0).
        assert_ne!(compute_hash(&float_one), compute_hash(&int_one));
    }

    #[test]
    fn hashable_value_float_display() {
        // Display shows the f64 value, not the raw bits.
        let v = HashableValue::Float(3.14f64.to_bits());
        assert_eq!(v.to_string(), "3.14");

        let nan = HashableValue::Float(f64::NAN.to_bits());
        assert_eq!(nan.to_string(), "NaN");
    }
}

#[cfg(test)]
mod groupspine_tests {
    use super::*;

    fn make_thunk(n: i64) -> Arc<Thunk> {
        Arc::new(Thunk::value(
            Value::Int {
                n,
                type_val: unknown_type_val(),
            },
            crate::rust_span!(),
        ))
    }

    #[test]
    fn test_empty_spine() {
        let spine = GroupSpine::empty();
        assert_eq!(spine.len(), 0);
        assert!(spine.is_empty());
        assert!(spine.get(0).is_none());
    }

    #[test]
    fn test_from_flat() {
        let t0 = make_thunk(10);
        let t1 = make_thunk(20);
        let spine = GroupSpine::from_flat(vec![Arc::clone(&t0), Arc::clone(&t1)]);
        assert_eq!(spine.len(), 2);
        assert!(!spine.is_empty());
        // Verify identity: get returns the exact Arc that was inserted
        assert!(Arc::ptr_eq(&spine.get(0).unwrap(), &t0));
        assert!(Arc::ptr_eq(&spine.get(1).unwrap(), &t1));
        assert!(spine.get(2).is_none());
    }

    #[test]
    fn test_extend_single_level() {
        let base = GroupSpine::empty();
        let t0 = make_thunk(1);
        let spine = base.extend(vec![Arc::clone(&t0)]);
        assert_eq!(spine.len(), 1);
        assert!(Arc::ptr_eq(&spine.get(0).unwrap(), &t0));
        assert!(spine.get(1).is_none());
    }

    #[test]
    fn test_extend_multi_level() {
        let t0 = make_thunk(100);
        let t1 = make_thunk(200);
        let t2 = make_thunk(300);
        let base = GroupSpine::from_flat(vec![Arc::clone(&t0)]);
        let mid = base.extend(vec![Arc::clone(&t1)]);
        let top = mid.extend(vec![Arc::clone(&t2)]);

        assert_eq!(top.len(), 3);
        // Verify each slot returns the correct thunk (not just any present thunk)
        assert!(Arc::ptr_eq(&top.get(0).unwrap(), &t0)); // from base
        assert!(Arc::ptr_eq(&top.get(1).unwrap(), &t1)); // from mid
        assert!(Arc::ptr_eq(&top.get(2).unwrap(), &t2)); // from top
        assert!(top.get(3).is_none());

        // Earlier levels unaffected — structural sharing doesn't corrupt prior spines
        assert_eq!(base.len(), 1);
        assert!(Arc::ptr_eq(&base.get(0).unwrap(), &t0));
        assert!(base.get(1).is_none());

        assert_eq!(mid.len(), 2);
        assert!(Arc::ptr_eq(&mid.get(0).unwrap(), &t0));
        assert!(Arc::ptr_eq(&mid.get(1).unwrap(), &t1));
        assert!(mid.get(2).is_none());
    }

    #[test]
    fn test_extend_empty_returns_same_arc() {
        let spine = GroupSpine::from_flat(vec![make_thunk(1)]);
        let extended = spine.extend(vec![]);
        // extend with empty returns Arc::clone of self
        assert!(Arc::ptr_eq(&spine, &extended));
    }

    #[test]
    fn test_boundary_get() {
        let t = make_thunk(42);
        let base = GroupSpine::from_flat(vec![Arc::clone(&t)]);
        // Boundary: slot 0 present, slot 1 absent
        assert!(Arc::ptr_eq(&base.get(0).unwrap(), &t));
        assert!(base.get(1).is_none());
        // After extend, new entry at slot 1 — old slot 0 still maps to t
        let t2 = make_thunk(99);
        let ext = base.extend(vec![Arc::clone(&t2)]);
        assert!(Arc::ptr_eq(&ext.get(0).unwrap(), &t));
        assert!(Arc::ptr_eq(&ext.get(1).unwrap(), &t2));
        assert!(ext.get(2).is_none());
    }
}
