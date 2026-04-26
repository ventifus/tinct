//! Runtime value types: `Value`, `Thunk` (lazy memoization), `Environment` (lexical scope chain).

use std::borrow::Cow;
use std::cell::{Ref, RefCell};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{Expr, Param, Span, Spanned};
use crate::error::{EvalError, EvalResult};
use crate::types::Type;

/// Arguments passed to built-in functions.
pub struct BuiltinArgs<'a> {
    pub args: &'a [Rc<Thunk>],
    pub named: &'a IndexMap<String, Rc<Thunk>>,
    pub depth: usize,
    pub call_span: Span,
    pub ctx: Rc<crate::eval::EvalContext>,
}

/// Signature for built-in functions: receives a `BuiltinArgs` struct containing
/// positional args, named args, evaluation depth, and call-site span.
/// Returns an `Rc<Thunk>` to allow builtins to participate in lazy evaluation.
pub type BuiltinFn = fn(BuiltinArgs) -> EvalResult<Rc<Thunk>>;

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
    Dict(IndexMap<Key, Rc<Thunk>>),
    /// User-defined function (closure capturing its defining environment)
    Function {
        params: Rc<Vec<Param>>,
        body: Rc<Spanned<Expr>>,
        env: Rc<RefCell<Environment>>,
    },
    /// Rust-native built-in function
    Builtin { name: &'static str, func: BuiltinFn },
    /// Lazy linked-list sequence (head element, tail sequence)
    Seq { head: Rc<Thunk>, tail: Rc<Thunk> },
    /// Proxy object — field access calls the handler function with the field name
    Proxy { handler: Rc<Thunk> },
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
            Value::Builtin { .. } => "Builtin",
            Value::Seq { .. } => "Seq",
            Value::Proxy { .. } => "Proxy",
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
            Value::Builtin { name, .. } => write!(f, "Builtin({name})"),
            Value::Seq { .. } => write!(f, "Seq(...)"),
            Value::Proxy { .. } => write!(f, "Proxy"),
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
            Value::Builtin { name, .. } => write!(f, "<builtin {name}>"),
            Value::Seq { .. } => write!(f, "Seq(...)"),
            Value::Proxy { .. } => write!(f, "<proxy>"),
        }
    }
}

/// Compares primitives (Int, Float, String, Bool) by value; cross-variant
/// comparison always returns false (e.g. `Int(1) != Float(1.0)`). Float uses
/// IEEE 754 semantics (NaN != NaN). Dict, Function, Builtin, and Seq are
/// intentionally non-comparable and always return false, even to themselves.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            // Dict, Function, Builtin, and Seq are not structurally compared
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ThunkState {
    Unevaluated {
        expr: Rc<Spanned<Expr>>,
        env: Rc<RefCell<Environment>>,
        ctx: Rc<crate::eval::EvalContext>,
    },
    PendingBuiltin {
        func: BuiltinFn,
        args: Vec<Rc<Thunk>>,
        named: IndexMap<String, Rc<Thunk>>,
        depth: usize,
        call_span: Span,
        ctx: Rc<crate::eval::EvalContext>,
    },
    PendingCall {
        func: Rc<Thunk>,
        args: Vec<Rc<Thunk>>,
        named: IndexMap<String, Rc<Thunk>>,
        call_span: Span,
        ctx: Rc<crate::eval::EvalContext>,
    },
    Guarded {
        inner: Rc<Thunk>,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
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
    /// Used for stack trace construction when materialization fails.
    pub(crate) origin: Cow<'static, str>,
}

impl Thunk {
    pub fn new_unevaluated(
        expr: Rc<Spanned<Expr>>,
        env: Rc<RefCell<Environment>>,
        ctx: Rc<crate::eval::EvalContext>,
        span: Span,
    ) -> Self {
        Self {
            state: RefCell::new(ThunkState::Unevaluated { expr, env, ctx }),
            span,
            origin: Cow::Borrowed(""),
        }
    }

    pub fn new_materialized(value: Value, span: Span) -> Self {
        Self {
            state: RefCell::new(ThunkState::Materialized(value)),
            span,
            origin: Cow::Borrowed(""),
        }
    }

    pub fn new_pending_builtin(
        func: BuiltinFn,
        args: Vec<Rc<Thunk>>,
        named: IndexMap<String, Rc<Thunk>>,
        depth: usize,
        span: Span,
        origin: Cow<'static, str>,
        ctx: Rc<crate::eval::EvalContext>,
    ) -> Self {
        Self {
            state: RefCell::new(ThunkState::PendingBuiltin {
                func,
                args,
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
        span: Span,
        origin: Cow<'static, str>,
        ctx: Rc<crate::eval::EvalContext>,
    ) -> Self {
        Self {
            state: RefCell::new(ThunkState::PendingCall {
                func,
                args,
                named,
                call_span,
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
        let origin = if field_path.is_empty() {
            Cow::Borrowed("type guard")
        } else {
            Cow::Owned(format!("type guard: {}", field_path.join(".")))
        };
        Self {
            state: RefCell::new(ThunkState::Guarded {
                inner,
                expected,
                field_path,
                guard_span,
            }),
            span: guard_span,
            origin,
        }
    }

    /// Set the origin label for this thunk (used in stack traces).
    pub fn with_origin(mut self, label: Cow<'static, str>) -> Self {
        self.origin = label;
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
        BuiltinFn,
        Vec<Rc<Thunk>>,
        IndexMap<String, Rc<Thunk>>,
        usize,
        Span,
        Rc<crate::eval::EvalContext>,
    )> {
        let mut state = self.state.borrow_mut();
        match std::mem::replace(&mut *state, ThunkState::InProgress) {
            ThunkState::PendingBuiltin {
                func,
                args,
                named,
                depth,
                call_span,
                ctx,
            } => Some((func, args, named, depth, call_span, ctx)),
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
        IndexMap<String, Rc<Thunk>>,
        Span,
        Rc<crate::eval::EvalContext>,
    )> {
        let mut state = self.state.borrow_mut();
        match std::mem::replace(&mut *state, ThunkState::InProgress) {
            ThunkState::PendingCall {
                func,
                args,
                named,
                call_span,
                ctx,
            } => Some((func, args, named, call_span, ctx)),
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
    pub fn take_guarded(&self) -> Option<(Rc<Thunk>, Type, Vec<String>, Span)> {
        let mut state = self.state.borrow_mut();
        match std::mem::replace(&mut *state, ThunkState::InProgress) {
            ThunkState::Guarded {
                inner,
                expected,
                field_path,
                guard_span,
            } => Some((inner, expected, field_path, guard_span)),
            other => {
                *state = other;
                None
            }
        }
    }

    /// Cache a failed evaluation by transitioning to the Failed state.
    /// Used to memoize errors so failed thunks don't re-evaluate on subsequent access.
    pub fn cache_failure(&self, err: &EvalError) {
        self.transition(|_| ThunkState::Failed(Box::new(err.clone())));
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
        if !self.origin.is_empty() {
            s.field("origin", &self.origin);
        }
        s.finish()
    }
}

/// Lexical scope chain: bindings in the current scope plus an optional parent link.
#[derive(Debug, Clone)]
pub struct Environment {
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
        crate::eval::EvalContext::new(
            std::path::PathBuf::from("."),
            Rc::new(RefCell::new(Environment::new())),
            false,
        )
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
        let b = Value::Builtin {
            name: "test",
            func: dummy,
        };
        assert_ne!(b.clone(), b);
    }

    #[test]
    fn test_seq_type_name() {
        let seq = Value::Seq {
            head: Rc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 1),
            )),
            tail: Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                test_span(1, 1, 1, 1),
            )),
        };
        assert_eq!(seq.type_name(), "Seq");
    }

    #[test]
    fn test_seq_debug() {
        let seq = Value::Seq {
            head: Rc::new(Thunk::new_materialized(
                Value::Int(1),
                test_span(1, 1, 1, 1),
            )),
            tail: Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                test_span(1, 1, 1, 1),
            )),
        };
        let debug_str = format!("{:?}", seq);
        assert_eq!(debug_str, "Seq(...)");
    }

    #[test]
    fn test_seq_display() {
        let seq = Value::Seq {
            head: Rc::new(Thunk::new_materialized(
                Value::Int(1),
                test_span(1, 1, 1, 1),
            )),
            tail: Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                test_span(1, 1, 1, 1),
            )),
        };
        let display_str = format!("{}", seq);
        assert_eq!(display_str, "Seq(...)");
    }

    #[test]
    fn test_seq_not_equal_to_itself() {
        let seq = Value::Seq {
            head: Rc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 1),
            )),
            tail: Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                test_span(1, 1, 1, 1),
            )),
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
        let mut map = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        map.insert(
            Key::String("x".into()),
            Rc::new(Thunk::new_materialized(Value::Int(1), span)),
        );
        map.insert(
            Key::Int(0),
            Rc::new(Thunk::new_materialized(Value::Int(2), span)),
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
        let builtin = Value::Builtin {
            name: "test_fn",
            func: dummy_builtin,
        };
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
        let mut map = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        map.insert(
            Key::String("x".into()),
            Rc::new(Thunk::new_materialized(Value::Int(1), span)),
        );
        map.insert(
            Key::Int(0),
            Rc::new(Thunk::new_materialized(Value::Int(2), span)),
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
        let builtin = Value::Builtin {
            name: "test_builtin",
            func: dummy_builtin,
        };
        assert_eq!(format!("{builtin:?}"), "Builtin(test_builtin)");
    }

    #[test]
    fn test_thunk_unevaluated_preserves_ctx_across_materialization() {
        use crate::ast::Expr;

        // Create ctx1 with a distinct base_dir
        let ctx1 = crate::eval::EvalContext::new(
            std::path::PathBuf::from("/test/path/1"),
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
        let ctx1 = crate::eval::EvalContext::new(
            std::path::PathBuf::from("/test/path/builtin"),
            Rc::new(RefCell::new(Environment::new())),
            false,
        );

        let span = test_span(1, 1, 1, 5);
        let thunk = Thunk::new_pending_builtin(
            dummy_builtin,
            vec![],
            IndexMap::new(),
            0,
            span,
            Cow::Borrowed("test builtin call"),
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

        let (_func, _args, _named, _depth, _call_span, taken_ctx) = taken.unwrap();
        assert!(
            Rc::ptr_eq(&taken_ctx, &ctx1),
            "PendingBuiltin should evaluate using captured ctx1"
        );
    }

    #[test]
    fn test_thunk_pending_call_preserves_ctx() {
        // Create ctx1
        let ctx1 = crate::eval::EvalContext::new(
            std::path::PathBuf::from("/test/path/call"),
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
            span,
            Cow::Borrowed("test call"),
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

        let (_func, _args, _named, _call_span, taken_ctx) = taken.unwrap();
        assert!(
            Rc::ptr_eq(&taken_ctx, &ctx1),
            "PendingCall should evaluate using captured ctx1"
        );
    }
}
