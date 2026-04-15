// Evaluator foundation -- used starting Phase 1b
#![allow(dead_code)]

use std::cell::{Ref, RefCell};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{Annotation, Expr, Param, Span, Spanned};
use crate::error::EvalError;

pub type BuiltinFn = fn(&[Rc<Thunk>], &IndexMap<String, Rc<Thunk>>) -> Result<Value, EvalError>;

// --- Key ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Int(i64),
    String(String),
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

// --- Value ---

#[derive(Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    String(String),
    Bool(bool),
    Dict(IndexMap<Key, Rc<Thunk>>),
    Function {
        params: Vec<Param>,
        body: Spanned<Expr>,
        env: Rc<RefCell<Environment>>,
        return_ann: Option<Spanned<Annotation>>,
    },
    Builtin {
        name: &'static str,
        func: BuiltinFn,
    },
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
        }
    }
}

/// Compares primitives (Int, Float, String, Bool) by value; cross-variant
/// comparison always returns false (e.g. `Int(1) != Float(1.0)`). Float uses
/// IEEE 754 semantics (NaN != NaN). Dict, Function, and Builtin are
/// intentionally non-comparable and always return false, even to themselves.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            // Dict, Function, and Builtin are not structurally compared
            _ => false,
        }
    }
}

// --- ThunkState ---

#[derive(Debug, Clone)]
pub enum ThunkState {
    Unevaluated,
    InProgress,
    Materialized(Value),
}

// --- Thunk ---

pub struct Thunk {
    state: RefCell<ThunkState>,
    pub span: Span,
}

impl Thunk {
    pub fn new_unevaluated(span: Span) -> Self {
        Self {
            state: RefCell::new(ThunkState::Unevaluated),
            span,
        }
    }

    pub fn new_materialized(value: Value, span: Span) -> Self {
        Self {
            state: RefCell::new(ThunkState::Materialized(value)),
            span,
        }
    }

    pub fn state(&self) -> Ref<ThunkState> {
        self.state.borrow()
    }

    pub fn set_state(&self, state: ThunkState) {
        *self.state.borrow_mut() = state;
    }

    /// Atomically read the current state, compute a new state, and write it back.
    ///
    /// The closure receives an immutable reference to the current [`ThunkState`].
    /// The `Ref` from `borrow()` is dropped **before** `borrow_mut()` is called,
    /// so this avoids the double-borrow panic that occurs when callers write:
    ///
    /// ```ignore
    /// match &*thunk.state() {           // borrows
    ///     ThunkState::Unevaluated => {
    ///         thunk.set_state(InProgress); // borrow_mut while borrow is live → panic
    ///     }
    /// }
    /// ```
    ///
    /// Use this instead:
    ///
    /// ```ignore
    /// thunk.transition(|s| match s {
    ///     ThunkState::Unevaluated => ThunkState::InProgress,
    ///     other => other.clone(),
    /// });
    /// ```
    pub fn transition(&self, f: impl FnOnce(&ThunkState) -> ThunkState) {
        let new_state = f(&self.state.borrow());
        *self.state.borrow_mut() = new_state;
    }
}

impl fmt::Debug for Thunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.state.try_borrow() {
            Ok(state) => f
                .debug_struct("Thunk")
                .field("state", &*state)
                .field("span", &self.span)
                .finish(),
            Err(_) => f
                .debug_struct("Thunk")
                .field("state", &"<borrowed>")
                .field("span", &self.span)
                .finish(),
        }
    }
}

// --- Environment ---

#[derive(Debug, Clone)]
pub struct Environment {
    pub bindings: IndexMap<String, Rc<Thunk>>,
    pub parent: Option<Rc<RefCell<Environment>>>,
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
    /// infinite loop (and eventually a stack overflow).
    // TODO(Phase 1b): Convert recursive implementation to iterative to avoid stack overflow on deep scope chains
    pub fn get(&self, name: &str) -> Option<Rc<Thunk>> {
        if let Some(thunk) = self.bindings.get(name) {
            return Some(Rc::clone(thunk));
        }
        if let Some(parent) = &self.parent {
            return parent.borrow().get(name);
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
    use crate::ast::Position;

    fn test_span(start_line: usize, start_col: usize, end_line: usize, end_col: usize) -> Span {
        Span::new(
            Position {
                offset: 0,
                line: start_line,
                column: start_col,
            },
            Position {
                offset: 0,
                line: end_line,
                column: end_col,
            },
        )
    }

    // -- Key tests --

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

    // -- Value PartialEq tests --

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
            params: vec![],
            body: Spanned::new(Expr::Int(0), test_span(1, 1, 1, 1)),
            env: Rc::new(RefCell::new(Environment::new())),
            return_ann: None,
        };
        assert_ne!(f.clone(), f);
    }

    #[test]
    fn test_value_partial_eq_builtin_always_false() {
        fn dummy(_: &[Rc<Thunk>], _: &IndexMap<String, Rc<Thunk>>) -> Result<Value, EvalError> {
            Ok(Value::Int(0))
        }
        let b = Value::Builtin {
            name: "test",
            func: dummy,
        };
        assert_ne!(b.clone(), b);
    }

    // -- Environment tests --

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
        let parent_thunk = Rc::new(Thunk::new_materialized(Value::Int(1), span.clone()));
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

    // -- Thunk tests --

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
        let thunk = Thunk::new_unevaluated(span);

        // Verify initial state
        assert!(matches!(&*thunk.state(), ThunkState::Unevaluated));

        // Transition to InProgress
        thunk.transition(|s| match s {
            ThunkState::Unevaluated => ThunkState::InProgress,
            other => other.clone(),
        });

        assert!(matches!(&*thunk.state(), ThunkState::InProgress));
    }

    #[test]
    fn test_thunk_debug_borrowed_state() {
        let span = test_span(1, 1, 1, 5);
        let thunk = Thunk::new_unevaluated(span);

        // Hold a mutable borrow while formatting Debug
        let _guard = thunk.state.borrow_mut();
        let debug_str = format!("{:?}", thunk);

        // Should show "<borrowed>" instead of panicking
        assert!(
            debug_str.contains("<borrowed>"),
            "expected '<borrowed>' in debug output, got: {debug_str}"
        );
    }

    // -- Value::Display tests --

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
            Rc::new(Thunk::new_materialized(Value::Int(1), span.clone())),
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
        let params = vec![
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
        ];
        let body = Spanned::new(Expr::Int(0), span);
        let env = Rc::new(RefCell::new(Environment::new()));
        let func = Value::Function {
            params,
            body,
            env,
            return_ann: None,
        };
        assert_eq!(format!("{func}"), "[fn [x y] ...]");
    }

    #[test]
    fn test_value_display_builtin() {
        fn dummy_builtin(
            _args: &[Rc<Thunk>],
            _named: &IndexMap<String, Rc<Thunk>>,
        ) -> Result<Value, EvalError> {
            Ok(Value::Int(0))
        }
        let builtin = Value::Builtin {
            name: "test_fn",
            func: dummy_builtin,
        };
        assert_eq!(format!("{builtin}"), "<builtin test_fn>");
    }

    // -- Value::Debug tests --

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
            Rc::new(Thunk::new_materialized(Value::Int(1), span.clone())),
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
        let params = vec![
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
        ];
        let body = Spanned::new(Expr::Int(0), span);
        let env = Rc::new(RefCell::new(Environment::new()));
        let func = Value::Function {
            params,
            body,
            env,
            return_ann: None,
        };
        assert_eq!(format!("{func:?}"), "Function(a, b)");
    }

    #[test]
    fn test_value_debug_builtin() {
        fn dummy_builtin(
            _args: &[Rc<Thunk>],
            _named: &IndexMap<String, Rc<Thunk>>,
        ) -> Result<Value, EvalError> {
            Ok(Value::Int(0))
        }
        let builtin = Value::Builtin {
            name: "test_builtin",
            func: dummy_builtin,
        };
        assert_eq!(format!("{builtin:?}"), "Builtin(test_builtin)");
    }
}
