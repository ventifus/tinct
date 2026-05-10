//! AST types: `File`, `Document`, `Expr`, `Entry`, `Param`, `Annotation`, `Spanned<T>`.

use crate::types::Type;
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

/// Key type for dot access — either a string field name or an integer index.
#[derive(Debug, Clone, PartialEq)]
pub enum DotKey {
    Ident(String),
    Int(i64),
}

impl std::fmt::Display for DotKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DotKey::Ident(s) => write!(f, "{}", s),
            DotKey::Int(n) => write!(f, "{}", n),
        }
    }
}

/// Byte offset + line/column position in source text
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub offset: usize,
    pub line: usize,
    pub column: usize,
}

/// Source span (start..end)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: Position,
    pub end: Position,
}

impl Span {
    pub fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    /// A synthetic span representing the origin of the source text (offset 0, line 1, column 1).
    ///
    /// Used for errors generated outside of any particular source location, such as
    /// display depth limits and initial `%` values.
    pub fn origin() -> Self {
        let pos = Position {
            offset: 0,
            line: 1,
            column: 1,
        };
        Self::new(pos, pos)
    }
}

/// AST node with source location
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub fn new(node: T, span: Span) -> Self {
        Self { node, span }
    }
}

/// A complete LLT file -- one or more documents separated by ---
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    pub documents: Vec<Spanned<Document>>,
}

/// A document -- one or more expressions forming a scope chain
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub expressions: Vec<Rc<Spanned<Expr>>>,
    pub name: Option<String>,
    pub output_type: Option<Spanned<Annotation>>,
    pub expects: Option<Spanned<Annotation>>,
    pub caps: Option<Spanned<Vec<(String, Annotation)>>>,
}

/// The central expression type
///
/// PartialEq note: TypeAssert compares resolved_type RefCell — pre-typecheck vs post-typecheck
/// nodes will differ.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Integer literal, e.g. `42` or `-1`
    Int(i64),
    /// Float literal, e.g. `3.14`
    Float(f64),
    /// Boolean literal: `true` or `false`
    Bool(bool),
    /// Quoted string literal, e.g. `"hello"`. Bare words are `VarRef` in new syntax.
    Str(String),

    /// Variable reference. In new syntax, bare identifiers (`x`) and escaped refs (`$x`)
    /// both produce `VarRef`. The `%` pipeline variable is stored as `VarRef("%")`.
    ///
    /// The `escaped` field tracks whether this was written as `$x` (true) or `x` (false).
    /// This matters for pattern matching: `$x` in a pattern means "pin" (match against
    /// the value of x), while `x` in a pattern means "bind" (assign the matched value to x).
    ///
    /// The `resolved` field is populated by the variable resolution pass (Phase 1 of arena
    /// allocation strategy). It caches the (level, slot) de Bruijn coordinates for static
    /// bindings.
    ///
    /// Three-state sentinel:
    /// - Outer `None` — not yet processed by the resolution pass.
    /// - Outer `Some(None)` — processed, but unresolvable (e.g. `$include`-introduced bindings).
    /// - Outer `Some(Some((level, slot)))` — resolved to de Bruijn coordinates.
    ///
    /// This three-state representation allows the write-once invariant to catch
    /// double-resolution even for unresolvable variables (which would otherwise
    /// write `None` twice, indistinguishable from the initial unprocessed state).
    VarRef {
        name: String,
        /// True if written as `$name`, false if written as bare `name`
        escaped: bool,
        /// Resolved (level, slot) pair from variable resolution pass.
        /// Uses RefCell for write-once resolution without cloning the entire AST.
        /// The write-once invariant is enforced in resolve.rs.
        resolved: RefCell<Option<Option<(u32, u32)>>>,
    },
    /// Dot access on an expression, e.g. `$a.b` or `$a.0`
    DotAccess {
        expr: Box<Spanned<Expr>>,
        field: DotKey,
    },
    /// Pipe operator, e.g. `$a | f` (desugared before evaluation)
    Pipe {
        lhs: Box<Spanned<Expr>>,
        rhs: Box<Spanned<Expr>>,
    },

    /// Sequential expressions for multi-expression function bodies and match arms.
    /// Each expression's result dict extends the environment for subsequent expressions.
    /// The last expression's value is the overall result.
    /// Implements let* semantics (non-recursive, sequential bindings).
    Sequential(Vec<Rc<Spanned<Expr>>>),

    /// Dict/list literal with keyed and/or auto-indexed entries
    Dict(Vec<Spanned<Entry>>),

    /// Function application via `[call $f ...]` or `[f ...]`
    ///
    /// The `implied` flag preserves the author's choice for the formatter:
    /// - `implied: true` → `[f x y]` (implied call)
    /// - `implied: false` → `[call f x y]` (explicit call)
    Call {
        func: Box<Spanned<Expr>>,
        args: Vec<Rc<Spanned<Expr>>>,
        named_args: Vec<Spanned<NamedArg>>,
        implied: bool,
    },
    /// Function definition via `[fn [params] body]`
    ///
    /// When `desugared` is `true`, this node was synthesized by the `$_`
    /// desugaring pass (see `src/desugar.rs`) rather than written by the user.
    /// This follows Pombrio & Krishnamurthi (2014) origin tracking: tooling
    /// (LSP hover, error messages, formatters) can distinguish user-written
    /// lambdas from sugar-generated ones.
    Fn {
        return_ann: Option<Spanned<Annotation>>,
        params: Vec<Spanned<Param>>,
        body: Rc<Spanned<Expr>>,
        desugared: bool,
    },
    /// Type alias declaration via `[type expr]` or `[type [params] expr]`
    TypeAlias {
        params: Vec<String>,
        body: Box<Spanned<Expr>>,
    },

    /// Type assertion, e.g. `[@Number $expr]`
    TypeAssert {
        annotation: Spanned<Annotation>,
        expr: Box<Spanned<Expr>>,
        /// Type resolved during elaboration (type checking).
        /// Uses RefCell for write-once elaboration without cloning the entire AST.
        /// The write-once invariant is enforced in typecheck.rs `resolve_type_assert`.
        ///
        /// **WARNING:** `resolve_type_assert()` modifies AST in place. The AST is not
        /// thread-safe — typecheck and eval must not run concurrently on the same AST.
        /// LSP handles this by parsing fresh for each request.
        resolved_type: RefCell<Option<Type>>,
    },

    /// Annotated bare word in value position, e.g. `Fn@Number`
    Annotated {
        name: String,
        annotation: Spanned<Annotation>,
    },

    /// Row variable / open record marker, e.g. `...` or `...rest`
    Rest(Option<String>),

    /// Quote special form, e.g. `[quote expr]` — captures the AST of `expr` as a dict value.
    /// Phase 1 (opaque quote): no unquote handling yet.
    Quote(Box<Spanned<Expr>>),

    /// Unquote special form, e.g. `[unquote expr]` — evaluates `expr` and splices the result
    /// into a quoted AST. Only valid inside `[quote ...]`.
    Unquote(Box<Spanned<Expr>>),

    /// Unquote-splice special form, e.g. `[unquote-splice expr]` — evaluates `expr` (must be
    /// a list) and splices each element into the enclosing list position within a quoted AST.
    /// Only valid in list positions inside `[quote ...]` (not at top level of quote).
    UnquoteSplice(Box<Spanned<Expr>>),

    /// Macro definition, e.g. `[defmacro unless [fn [args] ...]]` — registers a compile-time
    /// transformer function. The macro expander evaluates `transformer`, registers it under
    /// `name`, and removes the DefMacro node from the AST before type-checking.
    DefMacro {
        name: String,
        transformer: Box<Spanned<Expr>>,
    },

    /// Pattern matching, e.g. `[match scrutinee pat1 body1 pat2 body2 ...]`
    Match {
        scrutinee: Box<Spanned<Expr>>,
        arms: Vec<MatchArm>,
    },

    /// Type class declaration, e.g. `[class [Equatable a] eq: [Fn@Bool [a a]]]`
    /// Declares a type class with type parameters and method signatures.
    ClassDecl {
        /// Class name (e.g., "Equatable")
        name: String,
        /// Type parameters (e.g., ["a"])
        params: Vec<String>,
        /// Superclass constraints (e.g., ["Ord"] for a class that requires Ord)
        superclasses: Vec<String>,
        /// Method signatures as dict entries (method_name: Type)
        methods: Vec<Spanned<Entry>>,
    },

    /// Type class instance declaration, e.g. `[instance [Equatable Int] eq: [fn [x y] [= x y]]]`
    /// Provides method implementations for a specific type.
    InstanceDecl {
        /// Class name (e.g., "Equatable")
        class_name: String,
        /// Instance type (e.g., Int, [name: Str age: Int])
        instance_type: Box<Spanned<Expr>>,
        /// Method implementations as dict entries (method_name: impl)
        methods: Vec<Spanned<Entry>>,
    },

    /// Parse error — a section of source that couldn't be parsed.
    /// Emitted by bracket-level error recovery (parser-rewrite.md §Phase 4).
    /// The span covers the entire unparseable region.
    Error(Span),
}

/// A dict entry (keyed or auto-indexed)
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: Option<Spanned<Expr>>,
    pub value: Rc<Spanned<Expr>>,
}

/// A named argument in a call expression
#[derive(Debug, Clone, PartialEq)]
pub struct NamedArg {
    pub name: String,
    pub value: Rc<Spanned<Expr>>,
}

/// A function parameter
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub annotation: Option<Spanned<Annotation>>,
    pub variadic: bool,
}

/// A match arm: pattern and corresponding body expression
#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: Spanned<Pattern>,
    /// Optional guard expression — evaluated after pattern matching succeeds.
    /// If the guard returns a falsy value, try the next arm.
    pub guard: Option<Box<Spanned<Expr>>>,
    pub body: Box<Spanned<Expr>>,
}

/// Pattern for match arms
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// Wildcard pattern `_` — always matches
    Wildcard,
    /// Variable binding pattern — lowercase bare word like `x` or `result`
    Variable(String),
    /// Literal pattern — int, float, bool, or string literal
    Literal(LiteralPattern),
    /// Type tag pattern — uppercase bare word like `Int`, `Str`, `Dict`
    TypeTag(String),
    /// Pin pattern — `$name` matches against existing variable value
    Pin(String),
    /// Dict pattern — matches dicts by key, binds matched values to pattern variables
    /// `rest: true` means open matching (extra keys allowed)
    /// `rest: false` means closed matching (extra keys rejected)
    Dict {
        fields: Vec<(String, Spanned<Pattern>)>,
        rest: bool,
    },
    /// Seq pattern — matches Seq values, binds head and tail
    /// `[seq h t]` desugars to `Seq { head: Pattern::Variable("h"), tail: Pattern::Variable("t") }`
    Seq {
        head: Box<Spanned<Pattern>>,
        tail: Box<Spanned<Pattern>>,
    },
    /// Constructor pattern — matches nominal variants by tag, binds payload
    /// `[Some v]` matches `Variant { tag: "Some", payload }` and binds `v` to the payload
    /// `None` (handled as TypeTag currently) matches `Variant { tag: "None", payload: None }`
    Constructor {
        tag: String,
        binding: Option<Box<Spanned<Pattern>>>,
    },
    /// Or-pattern — matches if any sub-pattern matches
    /// Both branches must bind the same set of variables
    Or(Vec<Spanned<Pattern>>),
}

/// Literal pattern values
#[derive(Debug, Clone, PartialEq)]
pub enum LiteralPattern {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}

/// An annotation (type shorthand or property dict)
#[derive(Debug, Clone, PartialEq)]
pub enum Annotation {
    Simple(String),
    PropertyDict(Vec<Spanned<Entry>>),
}

impl Annotation {
    /// Look up a property by string key in a PropertyDict annotation.
    /// Returns a reference to the value expression if found, None for Simple annotations.
    pub fn get_property(&self, key: &str) -> Option<&Spanned<Expr>> {
        match self {
            Annotation::PropertyDict(entries) => entries.iter().find_map(|entry| {
                let key_expr = entry.node.key.as_ref()?;
                match &key_expr.node {
                    Expr::Str(name) if name == key => Some(entry.node.value.as_ref()),
                    _ => None,
                }
            }),
            Annotation::Simple(_) => None,
        }
    }
}

impl fmt::Display for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, doc) in self.documents.iter().enumerate() {
            if i > 0 {
                write!(f, "\n---\n")?;
            }
            write!(f, "{}", doc.node)?;
        }
        Ok(())
    }
}

impl fmt::Display for Document {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, expr) in self.expressions.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{}", expr.node)?;
        }
        Ok(())
    }
}

/// Authoritative display representation for error messages. This is the canonical
/// rendering of an `Expr` node used in type errors, eval errors, and diagnostics.
/// For pretty-printing of source code, see `src/formatter.rs` (`Formatter` struct),
/// which preserves whitespace and formatting from the original source.
impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Int(n) => write!(f, "{n}"),
            Expr::Float(n) => write!(f, "{n}"),
            Expr::Bool(b) => write!(f, "{b}"),
            Expr::Str(s) => write!(f, "{s:?}"),
            // Emit name as-is. `%`-prefixed refs already include `%` in the name.
            // Plain identifiers and (indistinguishable) EscapedRefs both display without `$` —
            // Display is used for error messages, not source roundtripping.
            Expr::VarRef { name, .. } => write!(f, "{name}"),
            Expr::DotAccess { expr, field } => match field {
                DotKey::Ident(s) => write!(f, "{}.{s}", expr.node),
                DotKey::Int(n) => write!(f, "{}.{n}", expr.node),
            },
            Expr::Pipe { lhs, rhs } => write!(f, "{} | {}", lhs.node, rhs.node),
            Expr::Sequential(exprs) => {
                write!(f, "(seq")?;
                for expr in exprs {
                    write!(f, " {}", expr.node)?;
                }
                write!(f, ")")
            }
            Expr::Dict(entries) => {
                write!(f, "[")?;
                for (i, entry) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, "  ")?;
                    }
                    if let Some(key) = &entry.node.key {
                        write!(f, "{}: {}", key.node, entry.node.value.node)?;
                    } else {
                        write!(f, "{}", entry.node.value.node)?;
                    }
                }
                write!(f, "]")
            }
            Expr::Call {
                func,
                args,
                named_args,
                implied,
            } => {
                if *implied {
                    write!(f, "[{}", func.node)?;
                } else {
                    write!(f, "[call {}", func.node)?;
                }
                for arg in args {
                    write!(f, " {}", arg.node)?;
                }
                for na in named_args {
                    write!(f, " {}: {}", na.node.name, na.node.value.node)?;
                }
                write!(f, "]")
            }
            Expr::Fn {
                return_ann,
                params,
                body,
                desugared: _,
            } => {
                write!(f, "[fn")?;
                if let Some(ann) = return_ann {
                    write!(f, "@{}", ann.node)?;
                }
                write!(f, " [")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    if p.node.variadic {
                        write!(f, "...")?;
                    }
                    write!(f, "{}", p.node.name)?;
                    if let Some(ann) = &p.node.annotation {
                        write!(f, "@{}", ann.node)?;
                    }
                }
                write!(f, "] {}]", body.as_ref().node)
            }
            Expr::TypeAlias { params, body } => {
                if params.is_empty() {
                    write!(f, "[type {}]", body.node)
                } else {
                    write!(f, "[type [{}] {}]", params.join(" "), body.node)
                }
            }
            Expr::TypeAssert {
                annotation, expr, ..
            } => {
                write!(f, "[@{} {}]", annotation.node, expr.node)
            }
            Expr::Annotated { name, annotation } => {
                write!(f, "{name}@{}", annotation.node)
            }
            Expr::Rest(None) => write!(f, "..."),
            Expr::Rest(Some(name)) => write!(f, "...{name}"),
            Expr::Quote(inner) => write!(f, "[quote {}]", inner.node),
            Expr::Unquote(inner) => write!(f, "[unquote {}]", inner.node),
            Expr::UnquoteSplice(inner) => write!(f, "[unquote-splice {}]", inner.node),
            Expr::DefMacro { name, transformer } => {
                write!(f, "[defmacro {} {}]", name, transformer.node)
            }
            Expr::Match { scrutinee, arms } => {
                write!(f, "[match {}", scrutinee.node)?;
                for arm in arms {
                    write!(f, " {} {}", arm.pattern.node, arm.body.node)?;
                }
                write!(f, "]")
            }
            Expr::ClassDecl {
                name,
                params,
                methods,
                ..
            } => {
                write!(f, "[class [{name}")?;
                for p in params {
                    write!(f, " {p}")?;
                }
                write!(f, "]")?;
                for entry in methods {
                    if let Some(key) = &entry.node.key {
                        write!(f, " {}: {}", key.node, entry.node.value.node)?;
                    }
                }
                write!(f, "]")
            }
            Expr::InstanceDecl {
                class_name,
                instance_type,
                methods,
            } => {
                write!(f, "[instance [{class_name} {}]", instance_type.node)?;
                for entry in methods {
                    if let Some(key) = &entry.node.key {
                        write!(f, " {}: {}", key.node, entry.node.value.node)?;
                    }
                }
                write!(f, "]")
            }
            Expr::Error(span) => write!(f, "<error at {span}>"),
        }
    }
}

impl fmt::Display for Annotation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Annotation::Simple(name) => write!(f, "{name}"),
            Annotation::PropertyDict(entries) => {
                write!(f, "[")?;
                for (i, entry) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, "  ")?;
                    }
                    if let Some(key) = &entry.node.key {
                        write!(f, "{}: {}", key.node, entry.node.value.node)?;
                    } else {
                        write!(f, "{}", entry.node.value.node)?;
                    }
                }
                write!(f, "]")
            }
        }
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pattern::Wildcard => write!(f, "_"),
            Pattern::Variable(name) => write!(f, "{name}"),
            Pattern::Literal(lit) => write!(f, "{lit}"),
            Pattern::TypeTag(tag) => write!(f, "{tag}"),
            Pattern::Pin(name) => write!(f, "${name}"),
            Pattern::Dict { fields, rest } => {
                write!(f, "[")?;
                for (i, (key, pat)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{}: {}", key, pat.node)?;
                }
                if *rest {
                    if !fields.is_empty() {
                        write!(f, " ")?;
                    }
                    write!(f, "...")?;
                }
                write!(f, "]")
            }
            Pattern::Seq { head, tail } => {
                write!(f, "[seq {} {}]", head.node, tail.node)
            }
            Pattern::Constructor { tag, binding } => {
                if let Some(pat) = binding {
                    write!(f, "[{} {}]", tag, pat.node)
                } else {
                    write!(f, "{}", tag)
                }
            }
            Pattern::Or(patterns) => {
                for (i, pat) in patterns.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    write!(f, "{}", pat.node)?;
                }
                Ok(())
            }
        }
    }
}

impl fmt::Display for LiteralPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LiteralPattern::Int(n) => write!(f, "{n}"),
            LiteralPattern::Float(n) => write!(f, "{n}"),
            LiteralPattern::Bool(b) => write!(f, "{b}"),
            LiteralPattern::Str(s) => write!(f, "{s:?}"),
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}-{}:{}",
            self.start.line, self.start.column, self.end.line, self.end.column
        )
    }
}

impl Expr {
    /// Helper constructor for VarRef with unresolved cache.
    /// Used throughout the codebase to create variable references before the resolution pass runs.
    /// Creates a bare (unescaped) variable reference: `x`, not `$x`.
    pub fn var_ref(name: String) -> Self {
        Expr::VarRef {
            name,
            escaped: false,
            // Outer None = not yet processed by the resolution pass.
            resolved: RefCell::new(None),
        }
    }

    /// Helper constructor for escaped VarRef (`$name`).
    /// Used in the parser when encountering Token::EscapedRef.
    /// In pattern context, escaped refs become pin patterns.
    pub fn escaped_ref(name: String) -> Self {
        Expr::VarRef {
            name,
            escaped: true,
            // Outer None = not yet processed by the resolution pass.
            resolved: RefCell::new(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::sp;

    #[test]
    fn test_display_type_assert() {
        let expr = Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Number".into())),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        };
        assert_eq!(format!("{expr}"), "[@Number 42]");
    }

    #[test]
    fn test_display_type_assert_with_property_dict() {
        // Annotation keys from the parser are always Expr::Str (bare words);
        // Expr::VarRef keys are structurally valid but never produced by the parser.
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: Rc::new(sp(Expr::Str("Number".into()))),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("min".into()))),
                value: Rc::new(sp(Expr::Int(0))),
            }),
        ];
        let expr = Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::var_ref("x".into()))),
            resolved_type: RefCell::new(None),
        };
        assert_eq!(format!("{expr}"), "[@[\"type\": \"Number\"  \"min\": 0] x]");
    }

    #[test]
    fn test_display_annotated() {
        let expr = Expr::Annotated {
            name: "Config".into(),
            annotation: sp(Annotation::Simple("ConfigType".into())),
        };
        assert_eq!(format!("{expr}"), "Config@ConfigType");
    }

    #[test]
    fn test_display_annotated_with_property_dict() {
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::var_ref("required".into()))),
            value: Rc::new(sp(Expr::Bool(true))),
        })];
        let expr = Expr::Annotated {
            name: "port".into(),
            annotation: sp(Annotation::PropertyDict(entries)),
        };
        assert_eq!(format!("{expr}"), "port@[required: true]");
    }

    #[test]
    fn test_display_call_no_args() {
        let expr = Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![],
            named_args: vec![],
            implied: false,
        };
        assert_eq!(format!("{expr}"), "[call f]");
    }

    #[test]
    fn test_display_call_with_args() {
        let expr = Expr::Call {
            func: Box::new(sp(Expr::var_ref("add".into()))),
            args: vec![Rc::new(sp(Expr::Int(1))), Rc::new(sp(Expr::Int(2)))],
            named_args: vec![],
            implied: false,
        };
        assert_eq!(format!("{expr}"), "[call add 1 2]");
    }

    #[test]
    fn test_display_call_with_named_args() {
        let expr = Expr::Call {
            func: Box::new(sp(Expr::var_ref("config".into()))),
            args: vec![],
            named_args: vec![sp(NamedArg {
                name: "port".into(),
                value: Rc::new(sp(Expr::Int(8080))),
            })],
            implied: false,
        };
        assert_eq!(format!("{expr}"), "[call config port: 8080]");
    }

    #[test]
    fn test_display_call_with_both_arg_types() {
        let expr = Expr::Call {
            func: Box::new(sp(Expr::var_ref("deploy".into()))),
            args: vec![Rc::new(sp(Expr::Str("prod".into())))],
            named_args: vec![sp(NamedArg {
                name: "replicas".into(),
                value: Rc::new(sp(Expr::Int(3))),
            })],
            implied: false,
        };
        assert_eq!(format!("{expr}"), "[call deploy \"prod\" replicas: 3]");
    }

    #[test]
    fn test_display_fn_no_params_no_return() {
        let expr = Expr::Fn {
            return_ann: None,
            params: vec![],
            body: Rc::new(sp(Expr::Int(42))),
            desugared: false,
        };
        assert_eq!(format!("{expr}"), "[fn [] 42]");
    }

    #[test]
    fn test_display_fn_with_params() {
        let expr = Expr::Fn {
            return_ann: None,
            params: vec![
                sp(Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                }),
                sp(Param {
                    name: "y".into(),
                    annotation: None,
                    variadic: false,
                }),
            ],
            body: Rc::new(sp(Expr::var_ref("x".into()))),
            desugared: false,
        };
        assert_eq!(format!("{expr}"), "[fn [x y] x]");
    }

    #[test]
    fn test_display_fn_with_return_annotation() {
        let expr = Expr::Fn {
            return_ann: Some(sp(Annotation::Simple("Number".into()))),
            params: vec![],
            body: Rc::new(sp(Expr::Int(0))),
            desugared: false,
        };
        assert_eq!(format!("{expr}"), "[fn@Number [] 0]");
    }

    #[test]
    fn test_display_fn_with_annotated_params() {
        let expr = Expr::Fn {
            return_ann: None,
            params: vec![sp(Param {
                name: "x".into(),
                annotation: Some(sp(Annotation::Simple("Int".into()))),
                variadic: false,
            })],
            body: Rc::new(sp(Expr::var_ref("x".into()))),
            desugared: false,
        };
        assert_eq!(format!("{expr}"), "[fn [x@Int] x]");
    }

    #[test]
    fn test_display_fn_with_variadic_param() {
        let expr = Expr::Fn {
            return_ann: None,
            params: vec![sp(Param {
                name: "args".into(),
                annotation: None,
                variadic: true,
            })],
            body: Rc::new(sp(Expr::var_ref("args".into()))),
            desugared: false,
        };
        assert_eq!(format!("{expr}"), "[fn [...args] args]");
    }

    #[test]
    fn test_display_dict_empty() {
        let expr = Expr::Dict(vec![]);
        assert_eq!(format!("{expr}"), "[]");
    }

    #[test]
    fn test_display_dict_with_keyed_entries() {
        let expr = Expr::Dict(vec![
            sp(Entry {
                key: Some(sp(Expr::var_ref("a".into()))),
                value: Rc::new(sp(Expr::Int(1))),
            }),
            sp(Entry {
                key: Some(sp(Expr::var_ref("b".into()))),
                value: Rc::new(sp(Expr::Int(2))),
            }),
        ]);
        assert_eq!(format!("{expr}"), "[a: 1  b: 2]");
    }

    #[test]
    fn test_display_dict_with_auto_indexed_entries() {
        let expr = Expr::Dict(vec![
            sp(Entry {
                key: None,
                value: Rc::new(sp(Expr::Int(10))),
            }),
            sp(Entry {
                key: None,
                value: Rc::new(sp(Expr::Int(20))),
            }),
        ]);
        assert_eq!(format!("{expr}"), "[10  20]");
    }

    #[test]
    fn test_display_annotation_simple() {
        let ann = Annotation::Simple("String".into());
        assert_eq!(format!("{ann}"), "String");
    }

    #[test]
    fn test_display_annotation_property_dict_empty() {
        let ann = Annotation::PropertyDict(vec![]);
        assert_eq!(format!("{ann}"), "[]");
    }

    #[test]
    fn test_display_annotation_property_dict_with_entries() {
        // Annotation keys from the parser are always Expr::Str (bare words);
        // Expr::VarRef keys are structurally valid but never produced by the parser.
        let ann = Annotation::PropertyDict(vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: Rc::new(sp(Expr::Str("Number".into()))),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: Rc::new(sp(Expr::Int(42))),
            }),
        ]);
        assert_eq!(format!("{ann}"), "[\"type\": \"Number\"  \"default\": 42]");
    }

    #[test]
    fn test_display_document_single_expression() {
        let doc = Document {
            expressions: vec![Rc::new(sp(Expr::Int(42)))],
            name: None,
            output_type: None,
            expects: None,
            caps: None,
        };
        assert_eq!(format!("{doc}"), "42");
    }

    #[test]
    fn test_display_document_multiple_expressions() {
        let doc = Document {
            expressions: vec![
                Rc::new(sp(Expr::var_ref("x".into()))),
                Rc::new(sp(Expr::Int(10))),
                Rc::new(sp(Expr::Bool(true))),
            ],
            name: None,
            output_type: None,
            expects: None,
            caps: None,
        };
        assert_eq!(format!("{doc}"), "x\n10\ntrue");
    }

    #[test]
    fn test_display_document_empty() {
        let doc = Document {
            expressions: vec![],
            name: None,
            output_type: None,
            expects: None,
            caps: None,
        };
        assert_eq!(format!("{doc}"), "");
    }

    #[test]
    fn test_display_file_single_document() {
        let file = File {
            documents: vec![sp(Document {
                expressions: vec![Rc::new(sp(Expr::Int(1)))],
                name: None,
                output_type: None,
                expects: None,
                caps: None,
            })],
        };
        assert_eq!(format!("{file}"), "1");
    }

    #[test]
    fn test_display_file_multiple_documents() {
        let file = File {
            documents: vec![
                sp(Document {
                    expressions: vec![Rc::new(sp(Expr::Int(1)))],
                    name: None,
                    output_type: None,
                    expects: None,
                    caps: None,
                }),
                sp(Document {
                    expressions: vec![Rc::new(sp(Expr::Int(2)))],
                    name: None,
                    output_type: None,
                    expects: None,
                    caps: None,
                }),
            ],
        };
        assert_eq!(format!("{file}"), "1\n---\n2");
    }

    #[test]
    fn test_display_rest_anonymous() {
        let expr = Expr::Rest(None);
        assert_eq!(format!("{expr}"), "...");
    }

    #[test]
    fn test_display_rest_named() {
        let expr = Expr::Rest(Some("extra".into()));
        assert_eq!(format!("{expr}"), "...extra");
    }

    #[test]
    fn test_display_error() {
        let span = Span::new(
            Position {
                offset: 10,
                line: 2,
                column: 5,
            },
            Position {
                offset: 20,
                line: 2,
                column: 15,
            },
        );
        let expr = Expr::Error(span);
        assert_eq!(format!("{expr}"), "<error at 2:5-2:15>");
    }
}
