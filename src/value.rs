//! Runtime value types: `Value`, `Thunk` (lazy memoization), `Environment` (lexical scope chain).

use std::cell::{Ref, RefCell};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use indexmap::{Equivalent, IndexMap};

use crate::ast::{Expr, Param, Span, Spanned};
use crate::error::{EvalError, EvalResult};
use crate::types::Type;

// Re-export ThunkId for use in other modules
pub use crate::arena::ThunkId;

/// Arguments passed to built-in functions.
pub struct BuiltinArgs<'a> {
    pub args: &'a [Rc<Thunk>],
    pub named: Option<&'a IndexMap<String, Rc<Thunk>>>,
    pub depth: usize,
    pub call_span: Span,
    pub ctx: Rc<crate::eval::EvalContext>,
}

/// Signature for built-in functions: receives a `BuiltinArgs` struct containing
/// positional args, named args, evaluation depth, and call-site span.
/// Returns an `Rc<Thunk>` to allow builtins to participate in lazy evaluation.
pub type BuiltinFn = fn(BuiltinArgs) -> EvalResult<Rc<Thunk>>;

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
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct BuiltinDef {
    pub func: BuiltinFn,
    pub name: &'static str,
    pub pos_strictness: &'static [Strictness],
}

impl fmt::Debug for BuiltinDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuiltinDef")
            .field("name", &self.name)
            .field("pos_strictness", &self.pos_strictness)
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
    // Future: IPv4/IPv6 CIDR ranges deferred to Phase 3
}

/// A materialized runtime value.
#[derive(Clone)]
pub enum Value {
    /// 64-bit signed integer
    Int(i64),
    /// 64-bit IEEE 754 float
    Float(f64),
    /// UTF-8 string (from bare words or quoted literals)
    String(String),
    /// Boolean (`true` or `false`)
    Bool(bool),
    /// Ordered key-value map with lazy (thunked) values
    Dict(IndexMap<Key, ThunkId>),
    /// User-defined function (closure capturing its defining environment)
    Function {
        params: Rc<Vec<Param>>,
        body: Rc<Spanned<Expr>>,
        env: Rc<RefCell<Environment>>,
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
    DirCap(Rc<cap_std::fs::Dir>),
    /// Network capability — authority to connect to specified hosts/subnets
    NetCap(Rc<Vec<NetCapEntry>>),
    /// Open file/stream handle (Read-only for Phase 1)
    Handle(Rc<std::cell::RefCell<Box<dyn std::io::BufRead>>>),
    /// Revocable directory capability
    RevocableDirCap {
        inner: Rc<cap_std::fs::Dir>,
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
}

impl Value {
    /// Returns the human-readable type name for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::String(_) => "String",
            Value::Bool(_) => "Bool",
            Value::Dict(_) => "Dict",
            Value::Function { .. } => "Function",
            Value::Builtin(_) => "Builtin",
            Value::Seq { .. } => "Seq",
            Value::Proxy { .. } => "Proxy",
            Value::Overlay(..) => "Dict",
            Value::DirCap(_) => "DirCap",
            Value::NetCap(_) => "NetCap",
            Value::Handle(_) => "Handle",
            Value::RevocableDirCap { .. } => "DirCap",
            Value::Variant { .. } => "Variant",
            Value::Decimal(_) => "Decimal",
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(n) => f.debug_tuple("Int").field(n).finish(),
            Value::Float(n) => f.debug_tuple("Float").field(n).finish(),
            Value::String(s) => f.debug_tuple("String").field(s).finish(),
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
            Value::DirCap(_) => write!(f, "DirCap"),
            Value::NetCap(entries) => write!(f, "NetCap({} entries)", entries.len()),
            Value::Handle(_) => write!(f, "Handle"),
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
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "{s:?}"),
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
            Value::DirCap(_) => write!(f, "<DirCap>"),
            Value::NetCap(_) => write!(f, "<NetCap>"),
            Value::Handle(_) => write!(f, "<Handle>"),
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
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Decimal(a), Value::Decimal(b)) => a == b,
            // Dict, Function, Builtin, Seq, Proxy, and Overlay are not structurally compared.
            // Overlay would require materializing both sides, breaking laziness.
            _ => false,
        }
    }
}

// Size assertion: ensure Value::Dict (IndexMap) remains the dominant variant.
// Value enum size is dominated by Dict(IndexMap). BuiltinDef is Copy (40 bytes:
// 8-byte fn ptr + 2 fat ptrs for &str and &[Strictness]). IndexMap size varies
// by version; indexmap 2.x uses ~72 bytes on 64-bit platforms.
const _: () = {
    const EXPECTED_MAX: usize = 80;
    const ACTUAL: usize = std::mem::size_of::<Value>();
    assert!(
        ACTUAL <= EXPECTED_MAX,
        "Value size increased beyond expected maximum"
    );
};

#[derive(Debug, Clone)]
pub enum ThunkState {
    /// Pre-allocation sentinel for letrec placeholder slots. Must be filled via
    /// `set_state()` with a real state (Unevaluated, Materialized, etc.) before
    /// any attempt to force/materialize. Forcing a Placeholder is a logic error
    /// indicating the letrec construction failed to fill all slots.
    ///
    /// Monotonicity: Placeholder → {Unevaluated, Materialized, PendingBuiltin, ...}
    /// is a forward state transition, unlike the previous Materialized(Bool(false)) →
    /// Unevaluated hack which violated Launchbury's monotonicity invariant.
    Placeholder,
    Unevaluated {
        expr: Rc<Spanned<Expr>>,
        env: Rc<RefCell<Environment>>,
        ctx: Rc<crate::eval::EvalContext>,
    },
    PendingBuiltin {
        def: BuiltinDef,
        args: Box<Vec<Rc<Thunk>>>,
        /// Named args for this builtin call. `None` means no named args (the common case);
        /// avoids allocating an empty `IndexMap` for the many internal `PendingBuiltin`
        /// thunks created by sequence generators and transforms.
        named: Option<IndexMap<String, Rc<Thunk>>>,
        depth: usize,
        call_span: Span,
        ctx: Rc<crate::eval::EvalContext>,
    },
    PendingCall {
        func: Rc<Thunk>,
        args: Box<Vec<Rc<Thunk>>>,
        /// Named args for this call. `None` means no named args (the common case);
        /// avoids allocating an empty `IndexMap` for positional-only calls.
        named: Option<Box<IndexMap<String, Rc<Thunk>>>>,
        call_span: Span,
        caller_env: Rc<RefCell<Environment>>,
        ctx: Rc<crate::eval::EvalContext>,
    },
    /// Wraps an inner thunk and validates its materialized value against an expected type.
    /// Carries no `ctx` field because it does not evaluate AST directly; it forces the
    /// inner thunk (which carries its own `ctx`) and then validates the result.
    /// `blame_label` tracks the typed/untyped boundary for gradual typing (co-natural strategy).
    Guarded {
        inner: Rc<Thunk>,
        expected: Type,
        field_path: Box<Vec<String>>,
        guard_span: Span,
        blame_label: Option<crate::error::BlameLabel>,
    },
    InProgress,
    Materialized(Value),
    Failed(Box<EvalError>),
}

/// Lazy evaluation cell: wraps an unevaluated expression, a pending builtin call,
/// or a materialized value with memoization (evaluate-at-most-once semantics).
pub struct Thunk {
    state: RefCell<ThunkState>,
    pub(crate) span: Span,
    /// Label describing this thunk's origin (e.g. "call $f").
    /// `None` for anonymous thunks (the common case); eliminates per-thunk String allocation.
    /// Used for stack trace construction when materialization fails.
    pub(crate) origin: Option<Rc<str>>,
}

impl Thunk {
    /// Create a placeholder thunk for letrec pre-allocation. Must be filled via
    /// `set_state()` before use. Panics at materialization if still in Placeholder state.
    pub fn new_placeholder(span: Span) -> Self {
        Self {
            state: RefCell::new(ThunkState::Placeholder),
            span,
            origin: None,
        }
    }

    pub fn new_unevaluated(
        expr: Rc<Spanned<Expr>>,
        env: Rc<RefCell<Environment>>,
        ctx: Rc<crate::eval::EvalContext>,
        span: Span,
    ) -> Self {
        Self {
            state: RefCell::new(ThunkState::Unevaluated { expr, env, ctx }),
            span,
            origin: None,
        }
    }

    pub fn new_materialized(value: Value, span: Span) -> Self {
        Self {
            state: RefCell::new(ThunkState::Materialized(value)),
            span,
            origin: None,
        }
    }

    /// `named`: pass `None` when there are no named args (the common case for internal
    /// thunks); pass `Some(map)` only when named args are actually present.
    pub fn new_pending_builtin(
        def: BuiltinDef,
        args: Vec<Rc<Thunk>>,
        named: Option<IndexMap<String, Rc<Thunk>>>,
        depth: usize,
        span: Span,
        origin: Option<Rc<str>>,
        ctx: Rc<crate::eval::EvalContext>,
    ) -> Self {
        Self {
            state: RefCell::new(ThunkState::PendingBuiltin {
                def,
                args: Box::new(args),
                named,
                depth,
                call_span: span,
                ctx,
            }),
            span,
            origin,
        }
    }

    pub fn new_pending_call(
        func: Rc<Thunk>,
        args: Vec<Rc<Thunk>>,
        named: IndexMap<String, Rc<Thunk>>,
        call_span: Span,
        caller_env: Rc<RefCell<Environment>>,
        span: Span,
        origin: Option<Rc<str>>,
        ctx: Rc<crate::eval::EvalContext>,
    ) -> Self {
        let named = if named.is_empty() {
            None
        } else {
            Some(Box::new(named))
        };
        Self {
            state: RefCell::new(ThunkState::PendingCall {
                func,
                args: Box::new(args),
                named,
                call_span,
                caller_env,
                ctx,
            }),
            span,
            origin,
        }
    }

    pub fn new_guarded(
        inner: Rc<Thunk>,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
    ) -> Self {
        Self::new_guarded_with_blame(inner, expected, field_path, guard_span, None)
    }

    pub fn new_guarded_with_blame(
        inner: Rc<Thunk>,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
        blame_label: Option<crate::error::BlameLabel>,
    ) -> Self {
        Self {
            state: RefCell::new(ThunkState::Guarded {
                inner,
                expected,
                field_path: Box::new(field_path),
                guard_span,
                blame_label,
            }),
            span: guard_span,
            origin: Some(Rc::from("type guard")),
        }
    }

    /// Set the origin label for this thunk (used in stack traces).
    pub fn with_origin(mut self, label: Rc<str>) -> Self {
        self.origin = Some(label);
        self
    }

    pub fn state(&self) -> Ref<ThunkState> {
        self.state.borrow()
    }

    /// Set the thunk state directly. Use this when the new state doesn't depend
    /// on the old state.
    pub fn set_state(&self, new_state: ThunkState) {
        *self.state.borrow_mut() = new_state;
    }

    pub fn try_get_materialized(&self) -> Option<Value> {
        match &*self.state.borrow() {
            ThunkState::Materialized(v) => Some(v.clone()),
            _ => None,
        }
    }

    /// Atomically read the current state, compute a new state, and write it back.
    ///
    /// The closure receives an immutable reference to the current [`ThunkState`].
    /// The `Ref` from `borrow()` is dropped **before** `borrow_mut()` is called,
    /// so this avoids the double-borrow panic that occurs when callers hold a
    /// `state()` borrow while trying to mutate:
    ///
    /// ```ignore
    /// // BAD: borrow_mut while borrow is live → panic
    /// match &*thunk.state() {
    ///     ThunkState::Unevaluated { .. } => { /* mutate thunk here */ }
    /// }
    /// ```
    ///
    /// Use `transition` instead:
    ///
    /// ```ignore
    /// thunk.transition(|s| match s {
    ///     ThunkState::Unevaluated { .. } => ThunkState::InProgress,
    ///     other => other.clone(),
    /// });
    /// ```
    pub fn transition(&self, f: impl FnOnce(&ThunkState) -> ThunkState) {
        let new_state = f(&self.state.borrow());
        *self.state.borrow_mut() = new_state;
    }

    /// Take ownership of unevaluated data, atomically setting state to InProgress.
    /// Returns None if the thunk is not in the Unevaluated state.
    #[allow(clippy::type_complexity)]
    pub fn take_unevaluated(
        &self,
    ) -> Option<(
        Rc<Spanned<Expr>>,
        Rc<RefCell<Environment>>,
        Rc<crate::eval::EvalContext>,
    )> {
        let mut state = self.state.borrow_mut();
        match std::mem::replace(&mut *state, ThunkState::InProgress) {
            ThunkState::Unevaluated { expr, env, ctx } => Some((expr, env, ctx)),
            other => {
                *state = other;
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
        Vec<Rc<Thunk>>,
        Option<IndexMap<String, Rc<Thunk>>>,
        usize,
        Span,
        Rc<crate::eval::EvalContext>,
    )> {
        let mut state = self.state.borrow_mut();
        match std::mem::replace(&mut *state, ThunkState::InProgress) {
            ThunkState::PendingBuiltin {
                def,
                args,
                named,
                depth,
                call_span,
                ctx,
            } => Some((def, *args, named, depth, call_span, ctx)),
            other => {
                *state = other;
                None
            }
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn take_pending_call(
        &self,
    ) -> Option<(
        Rc<Thunk>,
        Vec<Rc<Thunk>>,
        Option<IndexMap<String, Rc<Thunk>>>,
        Span,
        Rc<RefCell<Environment>>,
        Rc<crate::eval::EvalContext>,
    )> {
        let mut state = self.state.borrow_mut();
        match std::mem::replace(&mut *state, ThunkState::InProgress) {
            ThunkState::PendingCall {
                func,
                args,
                named,
                call_span,
                caller_env,
                ctx,
            } => Some((func, *args, named.map(|b| *b), call_span, caller_env, ctx)),
            other => {
                *state = other;
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
        Rc<Thunk>,
        Type,
        Vec<String>,
        Span,
        Option<crate::error::BlameLabel>,
    )> {
        let mut state = self.state.borrow_mut();
        match std::mem::replace(&mut *state, ThunkState::InProgress) {
            ThunkState::Guarded {
                inner,
                expected,
                field_path,
                guard_span,
                blame_label,
            } => Some((inner, expected, *field_path, guard_span, blame_label)),
            other => {
                *state = other;
                None
            }
        }
    }

    /// Cache a failed evaluation by transitioning to the Failed state.
    /// Used to memoize errors so failed thunks don't re-evaluate on subsequent access.
    ///
    /// Skips the clone and state write if the thunk is already in the Failed state
    /// (e.g., when a shared thunk is encountered a second time during error propagation).
    pub fn cache_failure(&self, err: &EvalError) {
        // Fast path: if already Failed, no work needed — avoid the clone.
        if matches!(&*self.state.borrow(), ThunkState::Failed(_)) {
            return;
        }
        self.set_state(ThunkState::Failed(Box::new(err.clone())));
    }
}

impl fmt::Debug for Thunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = match self.state.try_borrow() {
            Ok(state) => {
                let mut s = f.debug_struct("Thunk");
                s.field("state", &*state);
                s.field("span", &self.span);
                s
            }
            Err(_) => {
                let mut s = f.debug_struct("Thunk");
                s.field("state", &"<borrowed>");
                s.field("span", &self.span);
                s
            }
        };
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
    pub(crate) bindings: IndexMap<String, Rc<Thunk>>,
    pub(crate) parent: Option<Rc<RefCell<Environment>>>,
}

impl Environment {
    pub fn new() -> Self {
        Self {
            bindings: IndexMap::new(),
            parent: None,
        }
    }

    pub fn with_parent(parent: Rc<RefCell<Environment>>) -> Self {
        Self {
            bindings: IndexMap::new(),
            parent: Some(parent),
        }
    }

    /// Look up a binding by name, searching this environment then ancestors.
    ///
    /// # Borrow safety
    ///
    /// This method calls `borrow()` on each ancestor `RefCell<Environment>` as
    /// it walks up the scope chain.  Callers **must not** hold a mutable borrow
    /// (`borrow_mut()`) on any ancestor environment while calling `get()`, or
    /// the program will panic at runtime.
    ///
    /// The scope chain must form a DAG -- circular parent links will cause an
    /// infinite loop.
    pub fn get(&self, name: &str) -> Option<Rc<Thunk>> {
        // Check current scope first
        if let Some(thunk) = self.bindings.get(name) {
            return Some(Rc::clone(thunk));
        }
        // Walk parent chain iteratively
        let mut current = self.parent.as_ref().map(Rc::clone);
        while let Some(env_rc) = current {
            let env = env_rc.borrow();
            if let Some(thunk) = env.bindings.get(name) {
                return Some(Rc::clone(thunk));
            }
            current = env.parent.as_ref().map(Rc::clone);
        }
        None
    }

    pub fn insert(&mut self, name: String, thunk: Rc<Thunk>) {
        self.bindings.insert(name, thunk);
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

    fn test_ctx() -> Rc<crate::eval::EvalContext> {
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        crate::eval::EvalContext::new(base_dir, Rc::new(RefCell::new(Environment::new())), false)
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
        assert_eq!(Value::String("a".into()), Value::String("a".into()));
        assert_ne!(Value::String("a".into()), Value::String("b".into()));
        assert_eq!(Value::Bool(true), Value::Bool(true));
        assert_ne!(Value::Bool(true), Value::Bool(false));
    }

    #[test]
    fn test_value_partial_eq_cross_variant() {
        assert_ne!(Value::Int(1), Value::Float(1.0));
        assert_ne!(Value::Int(0), Value::Bool(false));
        assert_ne!(Value::String("1".into()), Value::Int(1));
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
            env: Rc::new(RefCell::new(Environment::new())),
        };
        assert_ne!(f.clone(), f);
    }

    #[test]
    fn test_value_partial_eq_builtin_always_false() {
        fn dummy(ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            Ok(Rc::new(Thunk::new_materialized(
                Value::Int(0),
                ctx.call_span,
            )))
        }
        let b = Value::Builtin(BuiltinDef {
            func: dummy,
            name: "test",
            pos_strictness: &[],
        });
        assert_ne!(b.clone(), b);
    }

    #[test]
    fn test_seq_type_name() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 1);
        let seq = Value::Seq {
            head: ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(42), span))),
            tail: ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
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
            head: ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(1), span))),
            tail: ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
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
            head: ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(1), span))),
            tail: ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
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
            head: ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(42), span))),
            tail: ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
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
        let thunk = Rc::new(Thunk::new_materialized(Value::Int(42), span));
        env.insert("x".into(), Rc::clone(&thunk));

        let found = env.get("x");
        assert!(found.is_some());
        assert!(Rc::ptr_eq(&found.unwrap(), &thunk));
    }

    #[test]
    fn test_environment_get_parent_scope() {
        let mut parent = Environment::new();
        let span = test_span(1, 1, 1, 5);
        let thunk = Rc::new(Thunk::new_materialized(Value::Int(99), span));
        parent.insert("y".into(), Rc::clone(&thunk));

        let parent_rc = Rc::new(RefCell::new(parent));
        let child = Environment::with_parent(Rc::clone(&parent_rc));

        let found = child.get("y");
        assert!(found.is_some());
        assert!(Rc::ptr_eq(&found.unwrap(), &thunk));
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
        let parent_thunk = Rc::new(Thunk::new_materialized(Value::Int(1), span));
        parent.insert("x".into(), Rc::clone(&parent_thunk));

        let parent_rc = Rc::new(RefCell::new(parent));
        let mut child = Environment::with_parent(parent_rc);
        let child_thunk = Rc::new(Thunk::new_materialized(Value::Int(2), span));
        child.insert("x".into(), Rc::clone(&child_thunk));

        let found = child.get("x").unwrap();
        // Should find the child's binding, not the parent's
        assert!(Rc::ptr_eq(&found, &child_thunk));
        assert!(!Rc::ptr_eq(&found, &parent_thunk));
    }

    #[test]
    fn test_thunk_new_materialized() {
        let span = test_span(1, 1, 1, 5);
        let thunk = Thunk::new_materialized(Value::Int(7), span);
        let state = thunk.state();
        match &*state {
            ThunkState::Materialized(v) => assert_eq!(*v, Value::Int(7)),
            other => panic!("expected Materialized, got {other:?}"),
        }
    }

    #[test]
    fn test_thunk_transition() {
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(0), span));
        let env = Rc::new(RefCell::new(Environment::new()));
        let thunk = Thunk::new_unevaluated(expr, env, test_ctx(), span);

        // Verify initial state
        assert!(matches!(&*thunk.state(), ThunkState::Unevaluated { .. }));

        // Transition to InProgress
        thunk.transition(|s| match s {
            ThunkState::Unevaluated { .. } => ThunkState::InProgress,
            other => other.clone(),
        });

        assert!(matches!(&*thunk.state(), ThunkState::InProgress));
    }

    #[test]
    fn test_thunk_debug_borrowed_state() {
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(0), span));
        let env = Rc::new(RefCell::new(Environment::new()));
        let thunk = Thunk::new_unevaluated(expr, env, test_ctx(), span);

        // Hold a mutable borrow while formatting Debug
        let _guard = thunk.state.borrow_mut();
        let debug_str = format!("{:?}", thunk);

        // Should show "<borrowed>" instead of panicking
        assert!(
            debug_str.contains("<borrowed>"),
            "expected '<borrowed>' in debug output, got: {debug_str}"
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
        assert_eq!(format!("{}", Value::String("hello".into())), "\"hello\"");
        assert_eq!(
            format!("{}", Value::String("with \"quotes\"".into())),
            "\"with \\\"quotes\\\"\""
        );
        assert_eq!(format!("{}", Value::String("".into())), "\"\"");
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
            ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(1), span))),
        );
        map.insert(
            Key::Int(0),
            ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(2), span))),
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
        let env = Rc::new(RefCell::new(Environment::new()));
        let func = Value::Function { params, body, env };
        assert_eq!(format!("{func}"), "[fn [x y] ...]");
    }

    #[test]
    fn test_value_display_builtin() {
        fn dummy_builtin(ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            Ok(Rc::new(Thunk::new_materialized(
                Value::Int(0),
                ctx.call_span,
            )))
        }
        let builtin = Value::Builtin(BuiltinDef {
            func: dummy_builtin,
            name: "test_fn",
            pos_strictness: &[],
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
        assert_eq!(
            format!("{:?}", Value::String("test".into())),
            "String(\"test\")"
        );
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
            ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(1), span))),
        );
        map.insert(
            Key::Int(0),
            ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(2), span))),
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
        let env = Rc::new(RefCell::new(Environment::new()));
        let func = Value::Function { params, body, env };
        assert_eq!(format!("{func:?}"), "Function(a, b)");
    }

    #[test]
    fn test_value_debug_builtin() {
        fn dummy_builtin(ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            Ok(Rc::new(Thunk::new_materialized(
                Value::Int(0),
                ctx.call_span,
            )))
        }
        let builtin = Value::Builtin(BuiltinDef {
            func: dummy_builtin,
            name: "test_builtin",
            pos_strictness: &[],
        });
        assert_eq!(format!("{builtin:?}"), "Builtin(test_builtin)");
    }

    #[test]
    fn test_thunk_unevaluated_preserves_ctx_across_materialization() {
        use crate::ast::Expr;

        // Create ctx1 with a distinct base_dir
        let base_dir1 = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let ctx1 = crate::eval::EvalContext::new(
            base_dir1,
            Rc::new(RefCell::new(Environment::new())),
            false,
        );

        // Create a thunk that captures ctx1
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(42), span));
        let env = Rc::new(RefCell::new(Environment::new()));
        let thunk =
            Thunk::new_unevaluated(Rc::clone(&expr), Rc::clone(&env), Rc::clone(&ctx1), span);

        // Verify the thunk captured ctx1 (before materialization)
        {
            let state = thunk.state();
            match &*state {
                ThunkState::Unevaluated {
                    expr: _,
                    env: _,
                    ctx,
                } => {
                    // Use Rc::ptr_eq to verify it's the SAME Rc, not just equal content
                    assert!(
                        Rc::ptr_eq(ctx, &ctx1),
                        "thunk should capture ctx1 before materialization"
                    );
                }
                other => panic!("expected Unevaluated state, got {other:?}"),
            }
        } // state guard dropped here

        // Materialize the thunk using ctx1 (simulating normal evaluation)
        // Note: materialize() is in eval.rs, but we can test the state transition
        // by calling take_unevaluated and verifying it returns the captured ctx
        let taken = thunk.take_unevaluated();
        assert!(
            taken.is_some(),
            "take_unevaluated should succeed on Unevaluated thunk"
        );

        let (_taken_expr, _taken_env, taken_ctx) = taken.unwrap();

        // Verify the taken ctx is the same Rc as ctx1
        assert!(
            Rc::ptr_eq(&taken_ctx, &ctx1),
            "thunk should evaluate using the ctx it captured at creation (ctx1)"
        );

        // Verify that the thunk is now InProgress (after take_unevaluated)
        {
            let state = thunk.state();
            match &*state {
                ThunkState::InProgress => {
                    // Expected: take_unevaluated sets state to InProgress
                }
                other => panic!("expected InProgress after take_unevaluated, got {other:?}"),
            }
        } // state guard dropped here
    }

    #[test]
    fn test_thunk_pending_builtin_preserves_ctx() {
        fn dummy_builtin(ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            Ok(Rc::new(Thunk::new_materialized(
                Value::Int(0),
                ctx.call_span,
            )))
        }

        // Create ctx1
        let base_dir1 = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let ctx1 = crate::eval::EvalContext::new(
            base_dir1,
            Rc::new(RefCell::new(Environment::new())),
            false,
        );

        let span = test_span(1, 1, 1, 5);
        let dummy_def = BuiltinDef {
            func: dummy_builtin,
            name: "test-builtin",
            pos_strictness: &[],
        };
        let thunk = Thunk::new_pending_builtin(
            dummy_def,
            vec![],
            None,
            0,
            span,
            Some(Rc::from("test builtin call")),
            Rc::clone(&ctx1),
        );

        // Verify the thunk captured ctx1
        match &*thunk.state() {
            ThunkState::PendingBuiltin { ctx, .. } => {
                assert!(Rc::ptr_eq(ctx, &ctx1), "PendingBuiltin should capture ctx1");
            }
            other => panic!("expected PendingBuiltin state, got {other:?}"),
        }

        // Take the pending builtin and verify ctx is preserved
        let taken = thunk.take_pending_builtin();
        assert!(taken.is_some(), "take_pending_builtin should succeed");

        let (_def, _args, _named, _depth, _call_span, taken_ctx) = taken.unwrap();
        assert!(
            Rc::ptr_eq(&taken_ctx, &ctx1),
            "PendingBuiltin should evaluate using captured ctx1"
        );
    }

    #[test]
    fn test_thunk_pending_call_preserves_ctx() {
        // Create ctx1
        let base_dir1 = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let ctx1 = crate::eval::EvalContext::new(
            base_dir1,
            Rc::new(RefCell::new(Environment::new())),
            false,
        );

        let span = test_span(1, 1, 1, 5);
        let func_thunk = Rc::new(Thunk::new_materialized(
            Value::Function {
                params: Rc::new(vec![]),
                body: Rc::new(Spanned::new(
                    crate::ast::Expr::Int(0),
                    test_span(1, 1, 1, 1),
                )),
                env: Rc::new(RefCell::new(Environment::new())),
            },
            span,
        ));

        let thunk = Thunk::new_pending_call(
            Rc::clone(&func_thunk),
            vec![],
            IndexMap::new(),
            span,
            Rc::new(RefCell::new(Environment::new())), // caller_env
            span,
            Some(Rc::from("test call")),
            Rc::clone(&ctx1),
        );

        // Verify the thunk captured ctx1
        match &*thunk.state() {
            ThunkState::PendingCall { ctx, .. } => {
                assert!(Rc::ptr_eq(ctx, &ctx1), "PendingCall should capture ctx1");
            }
            other => panic!("expected PendingCall state, got {other:?}"),
        }

        // Take the pending call and verify ctx is preserved
        let taken = thunk.take_pending_call();
        assert!(taken.is_some(), "take_pending_call should succeed");

        let (_func, _args, _named, _call_span, _caller_env, taken_ctx) = taken.unwrap();
        assert!(
            Rc::ptr_eq(&taken_ctx, &ctx1),
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
            handler: ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(42), span))),
        };
        assert_eq!(proxy.type_name(), "Proxy");
    }

    #[test]
    fn test_proxy_debug() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 1);
        let proxy = Value::Proxy {
            handler: ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(42), span))),
        };
        let debug_str = format!("{:?}", proxy);
        assert_eq!(debug_str, "Proxy");
    }

    #[test]
    fn test_proxy_display() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 1);
        let proxy = Value::Proxy {
            handler: ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(42), span))),
        };
        let display_str = format!("{}", proxy);
        assert_eq!(display_str, "<proxy>");
    }

    #[test]
    fn test_value_partial_eq_proxy_always_false() {
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 1);
        let p = Value::Proxy {
            handler: ctx.alloc_thunk(Rc::new(Thunk::new_materialized(Value::Int(42), span))),
        };
        assert_ne!(p.clone(), p);
    }

    #[test]
    fn test_thunk_new_guarded_state() {
        let span = test_span(1, 1, 1, 5);
        let inner = Rc::new(Thunk::new_materialized(Value::Int(42), span));
        let thunk = Thunk::new_guarded(
            Rc::clone(&inner),
            Type::Int,
            vec!["field".to_string()],
            span,
        );
        let state = thunk.state();
        match &*state {
            ThunkState::Guarded {
                expected,
                field_path,
                ..
            } => {
                assert_eq!(*expected, Type::Int);
                assert_eq!(field_path.as_ref(), &vec!["field".to_string()]);
            }
            other => panic!("expected Guarded state, got {other:?}"),
        }
    }

    #[test]
    fn test_take_guarded_returns_components() {
        let span = test_span(1, 1, 1, 5);
        let inner = Rc::new(Thunk::new_materialized(Value::Int(99), span));
        let thunk = Thunk::new_guarded(Rc::clone(&inner), Type::Int, vec!["x".to_string()], span);

        let result = thunk.take_guarded();
        assert!(
            result.is_some(),
            "take_guarded should succeed on Guarded thunk"
        );

        let (taken_inner, taken_expected, taken_path, _taken_span, _blame) = result.unwrap();
        assert!(
            Rc::ptr_eq(&taken_inner, &inner),
            "inner thunk should be the same Rc"
        );
        assert_eq!(taken_expected, Type::Int);
        assert_eq!(taken_path, vec!["x".to_string()]);

        // After take_guarded, thunk should be InProgress
        let state = thunk.state();
        match &*state {
            ThunkState::InProgress => {}
            other => panic!("expected InProgress after take_guarded, got {other:?}"),
        }
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
        let state = thunk.state();
        match &*state {
            ThunkState::Materialized(v) => assert_eq!(*v, Value::Int(7)),
            other => panic!("expected Materialized state to be preserved, got {other:?}"),
        }
    }

    #[test]
    fn test_thunk_new_guarded_fields() {
        let span = test_span(1, 1, 1, 5);
        let inner = Rc::new(Thunk::new_materialized(Value::Int(42), span));
        let thunk = Rc::new(Thunk::new_guarded(
            Rc::clone(&inner),
            Type::Int,
            vec!["foo".to_string()],
            span,
        ));
        let result = thunk.take_guarded();
        assert!(result.is_some());
        let (got_inner, got_type, got_path, _got_span, _blame) = result.unwrap();
        assert_eq!(got_path, vec!["foo".to_string()]);
        assert!(matches!(got_type, Type::Int));
        assert!(Rc::ptr_eq(&got_inner, &inner));
    }

    #[test]
    fn test_guarded_materialized_state_is_stable() {
        // Verifies that once a Guarded thunk is transitioned to Materialized,
        // the state is stable on re-access. Tests the state machine directly;
        // the full guard validation path (parse→eval→materialize) is covered
        // by test_guarded_thunk_preserves_inner_origin in eval.rs.
        let span = test_span(1, 1, 1, 5);
        let inner = Rc::new(Thunk::new_materialized(Value::Int(100), span));
        let thunk = Thunk::new_guarded(Rc::clone(&inner), Type::Int, vec![], span);

        // Verify initial state is Guarded
        {
            let state = thunk.state();
            assert!(
                matches!(&*state, ThunkState::Guarded { .. }),
                "initial state should be Guarded"
            );
        }

        // Directly transition to Materialized to verify state is stable on re-access.
        thunk.set_state(ThunkState::Materialized(Value::Int(100)));

        // Re-access: should return cached Materialized value
        let state = thunk.state();
        match &*state {
            ThunkState::Materialized(v) => assert_eq!(*v, Value::Int(100)),
            other => panic!("expected Materialized after guard success, got {other:?}"),
        }

        // try_get_materialized should also work
        drop(state);
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
        let ctx = EvalContext::new(base_dir, Rc::new(RefCell::new(Environment::new())), false);

        // Create a PendingBuiltin thunk (using a dummy builtin function)
        fn dummy_builtin(args: BuiltinArgs) -> crate::error::EvalResult<Rc<Thunk>> {
            let _ = args; // silence unused warning
            Ok(Rc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 1),
            )))
        }

        let dummy_def = BuiltinDef {
            func: dummy_builtin,
            name: "dummy",
            pos_strictness: &[],
        };
        let thunk = Thunk::new_pending_builtin(
            dummy_def,
            vec![],
            None,
            0,
            span,
            Some(Rc::from("test")),
            Rc::clone(&ctx),
        );

        // Verify initial state is PendingBuiltin
        {
            let state = thunk.state();
            assert!(
                matches!(&*state, ThunkState::PendingBuiltin { .. }),
                "initial state should be PendingBuiltin"
            );
        }

        // Transition to Materialized
        thunk.set_state(ThunkState::Materialized(Value::Int(42)));

        // Verify final state is Materialized
        let state = thunk.state();
        match &*state {
            ThunkState::Materialized(v) => assert_eq!(*v, Value::Int(42)),
            other => panic!("expected Materialized after builtin execution, got {other:?}"),
        }
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
        let ctx = EvalContext::new(base_dir, Rc::new(RefCell::new(Environment::new())), false);

        fn error_builtin(args: BuiltinArgs) -> crate::error::EvalResult<Rc<Thunk>> {
            Err(Box::new(EvalError::new(
                "test error".into(),
                args.call_span,
            )))
        }

        let error_def = BuiltinDef {
            func: error_builtin,
            name: "error_builtin",
            pos_strictness: &[],
        };
        let thunk = Thunk::new_pending_builtin(
            error_def,
            vec![],
            None,
            0,
            span,
            Some(Rc::from("test")),
            Rc::clone(&ctx),
        );

        // Transition to Failed
        let err = Box::new(EvalError::new("test error".into(), span));
        thunk.set_state(ThunkState::Failed(err));

        // Verify final state is Failed
        let state = thunk.state();
        match &*state {
            ThunkState::Failed(e) => assert!(e.message().contains("test error")),
            other => panic!("expected Failed state, got {other:?}"),
        }
    }
}
