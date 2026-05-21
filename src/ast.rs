//! AST types: `File`, `Document`, `Expr`, `Entry`, `Param`, `Annotation`, `Spanned<T>`.
//! Also: `SurfaceExpression`, `SurfaceNode`, `SurfaceProgram`, `CoreExpr` (runtime-v2 types).

use crate::types::Type;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Position {
    pub offset: usize,
    pub line: usize,
    pub column: usize,
}

/// Source span (start..end)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// Document stage — determines how the document is evaluated
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Runtime,
    Type,
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
    pub stage: Option<Stage>,
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

    /// Macro definition, e.g. `[defmacro unless [let pred] body...]` — registers a compile-time
    /// transformer function. Multi-body support: body expressions are wrapped in Sequential.
    /// The macro expander wraps this in a function, evaluates it, registers it under `name`,
    /// and removes the DefMacro node from the AST before type-checking.
    DefMacro {
        name: String,
        params: Rc<Spanned<Expr>>, // [let ...] pattern
        body: Rc<Spanned<Expr>>,
    },

    /// Macro declaration (macros-v2), e.g. `[macro my-if [let cond then else] body...]`
    /// Uses `[let ...]` pattern for parameter binding. The macro expander registers this
    /// and removes it from the AST before type-checking.
    MacroDecl {
        name: String,
        params: Box<Spanned<Expr>>, // [let ...] pattern (reuses existing LetDecl)
        body: Box<Spanned<Expr>>,
    },

    /// Multi-form splice (macros-v2), e.g. `[splice form1 form2 ...]`
    /// Only valid at dict top level. Injects multiple forms into the surrounding context.
    /// The expander removes this from the AST before type-checking.
    Splice(Vec<Spanned<Expr>>),

    /// Syntax class declaration (macros-v2), e.g. `[syntax-class pragma-name pattern: [let _ : VarRef] message: "..."]`
    /// Declares a named pattern validator for macro arguments. The expander registers this
    /// and removes it from the AST before type-checking.
    SyntaxClass {
        name: String,
        pattern: Box<Spanned<Expr>>,
        message: Option<String>,
    },

    /// Pattern matching, e.g. `[match scrutinee pat1 body1 pat2 body2 ...]`
    Match {
        scrutinee: Box<Spanned<Expr>>,
        arms: Vec<MatchArm>,
    },

    /// Type class declaration, e.g. `[class [Equatable a] eq: [Fn@Bool [a a]]]`
    /// Declares a type class with type parameters and method signatures.
    /// Extended form: `[class [a b c] [determines: [...] resolver: ...] methods...]`
    ClassDecl {
        /// Class name (e.g., "Equatable")
        name: String,
        /// Type parameters (e.g., ["a"])
        params: Vec<String>,
        /// Superclass constraints as (class_name, param_name) tuples.
        /// Example: ("Functor", "f") from `extends [Functor f]`
        superclasses: Vec<(String, String)>,
        /// Method signatures as dict entries (method_name: Type)
        methods: Vec<Spanned<Entry>>,
        /// Functional dependency declarations from `determines:` key in structural metadata bracket.
        /// Each entry is a two-element list: `[[determining-vars] determined-var]`
        /// Example: `[[[a b] c]]` means (a,b) determines c
        determines: Vec<Spanned<Expr>>,
        /// Resolver function name from `resolver:` key in structural metadata bracket.
        /// Names the type-stage function that computes determined types from determining types.
        resolver: Option<Box<Spanned<Expr>>>,
        /// Resolver injectivity flag from `injective:` key in structural metadata bracket.
        /// When true, the resolver is injective (enables congruence-based unification).
        resolver_injective: bool,
    },

    /// Type class instance declaration with match-arm syntax.
    /// New form: `[instance ClassName [pattern [...]]`: methods... ...]`
    /// Each arm pairs a pattern expression with method implementations.
    InstanceDecl {
        /// Class name (e.g., "Addable")
        class_name: String,
        /// Match arms: each is (pattern_expr, method_entries).
        /// Pattern expr is typically `Expr::PatternDecl` but can be any expr during parsing.
        /// Method entries are the method dict for that pattern.
        arms: Vec<(Spanned<Expr>, Vec<Spanned<Entry>>)>,
    },

    /// Type constructor application in annotation positions, e.g. `@[m a]`
    /// This is a type-level annotation node and should never be evaluated at runtime.
    /// The type checker disambiguates `[f a]` as TypeApp when `f` is Operator-kinded.
    /// Parsed from `@[...]` where the content is not a record type (no colons).
    TypeApp {
        func: Box<Spanned<Expr>>,
        arg: Box<Spanned<Expr>>,
    },

    /// Pattern declaration for instance match arms, e.g. `[pattern [a@Int b@Float c@Float]]`
    /// Bindings are typically `Expr::Annotated` nodes (var@Type).
    /// Used in `[instance Class [pattern ...]: methods ...]` syntax.
    PatternDecl {
        /// Pattern bindings — each is typically `Expr::Annotated { name, annotation }`
        bindings: Vec<Spanned<Expr>>,
    },

    /// Binding declaration list, e.g. `[let x@Int y@Float]`
    /// Used in fn params, class TypeVars, type alias params, instance arm keys, and case arms.
    /// Each element is one of:
    /// - VarRef(name) — bare binding
    /// - Annotated(name, ann) — typed binding (name@Type) or structural test (name: Constructor)
    /// - Wildcard (_) — matches anything, introduces no binding
    /// - LetDecl { .. } — nested bracket group for multi-payload patterns
    LetDecl { bindings: Vec<Spanned<Expr>> },

    /// Match arm with explicit scoping, e.g. `[case [let v: Ok] v]`
    /// Pattern can be either `Expr::LetDecl` (binding pattern) or any expression (exact-value match).
    CaseArm {
        pattern: Box<Spanned<Expr>>,
        body: Box<Spanned<Expr>>,
    },

    /// Placeholder expression `...` — evaluates to lazy error on force.
    /// Type: Unknown (satisfies any constraint).
    /// Eval: raises UnimplementedError when materialized.
    Placeholder,

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

impl std::fmt::Display for Param {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.variadic {
            write!(f, "...{}", self.name)?;
        } else {
            write!(f, "{}", self.name)?;
        }
        if let Some(ref ann) = self.annotation {
            write!(f, "@{}", ann.node)?;
        }
        Ok(())
    }
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
    Annotated(String, Box<Annotation>), // e.g., Seq@Int = Annotated("Seq", Simple("Int"))
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
            Annotation::Annotated(_, _) => None,
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
            Expr::Float(n) => {
                let s = n.to_string();
                if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                    write!(f, "{s}.0")
                } else {
                    write!(f, "{s}")
                }
            }
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
            Expr::DefMacro { name, params, body } => {
                write!(f, "[defmacro {} {} {}]", name, params.node, body.node)
            }
            Expr::MacroDecl { name, params, body } => {
                write!(f, "[macro {} {} {}]", name, params.node, body.node)
            }
            Expr::Splice(forms) => {
                write!(f, "[splice")?;
                for form in forms {
                    write!(f, " {}", form.node)?;
                }
                write!(f, "]")
            }
            Expr::SyntaxClass {
                name,
                pattern,
                message,
            } => {
                write!(f, "[syntax-class {} pattern: {}", name, pattern.node)?;
                if let Some(msg) = message {
                    write!(f, " message: {:?}", msg)?;
                }
                write!(f, "]")
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
            Expr::InstanceDecl { class_name, arms } => {
                write!(f, "[instance {class_name}")?;
                for (pattern, methods) in arms {
                    write!(f, " {}", pattern.node)?;
                    write!(f, ":")?;
                    for entry in methods {
                        if let Some(key) = &entry.node.key {
                            write!(f, " {}: {}", key.node, entry.node.value.node)?;
                        }
                    }
                }
                write!(f, "]")
            }
            Expr::TypeApp { func, arg } => write!(f, "@[{} {}]", func.node, arg.node),
            Expr::PatternDecl { bindings } => {
                write!(f, "[pattern")?;
                for binding in bindings {
                    write!(f, " {}", binding.node)?;
                }
                write!(f, "]")
            }
            Expr::LetDecl { bindings } => {
                write!(f, "[let")?;
                for binding in bindings {
                    write!(f, " {}", binding.node)?;
                }
                write!(f, "]")
            }
            Expr::CaseArm { pattern, body } => {
                write!(f, "[case {} {}]", pattern.node, body.node)
            }
            Expr::Placeholder => write!(f, "..."),
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
            Annotation::Annotated(name, inner) => write!(f, "{name}@{inner}"),
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
            LiteralPattern::Float(n) => {
                let s = n.to_string();
                if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                    write!(f, "{s}.0")
                } else {
                    write!(f, "{s}")
                }
            }
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

// ============================================================================
// runtime-v2 AST types (Sprint 1, Part A)
// ============================================================================
//
// SurfaceExpression / SurfaceNode / SurfaceProgram — immutable, Send+Sync, Arc-recursive.
// CoreExpr — evaluator-internal representation with de Bruijn coordinates.
// These coexist with Expr/Document/File until Part E removes the old types.

/// Dedicated wrapper for expression nodes in the Surface AST.
/// Identity is derived from the Arc pointer — no stored NodeId.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceNode {
    pub expr: SurfaceExpression,
    pub span: Span,
}

/// Pointer-derived node identity. Valid only while the owning Arc<SurfaceNode> is live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

/// Compute the NodeId for an Arc<SurfaceNode> from its raw pointer value.
pub fn node_id(arc: &Arc<SurfaceNode>) -> NodeId {
    NodeId(Arc::as_ptr(arc) as usize)
}

/// Immutable surface expression — what the parser produces and what tinct metaprogramming sees.
/// No RefCell fields. All recursive positions use Arc<SurfaceNode>.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceExpression {
    // Literals
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),

    // Variable reference. escaped: true = $name (pin in patterns), false = bare (bind).
    // No 'resolved' field — de Bruijn coordinates live in ResolutionTable keyed by NodeId.
    VarRef {
        name: String,
        escaped: bool,
    },

    // Access
    DotAccess {
        expr: Arc<SurfaceNode>,
        field: DotKey,
    },

    // Pipe is surface-only — the lowering pass rewrites it to Call before evaluation.
    // Kept here so formatters and metaprogramming can distinguish pipe-form from call-form.
    Pipe {
        lhs: Arc<SurfaceNode>,
        rhs: Arc<SurfaceNode>,
    },

    // Sequential let* scoping (multi-expr fn bodies, match arm bodies)
    Sequential(Vec<Arc<SurfaceNode>>),

    // Dict/list literal — key is None for auto-indexed (positional) entries
    Dict(Vec<Spanned<SurfaceEntry>>),

    // Function call — implied: true = [f x y], false = [call f x y]
    Call {
        func: Arc<SurfaceNode>,
        args: Vec<Arc<SurfaceNode>>,
        named_args: Vec<Spanned<SurfaceNamedArg>>,
        implied: bool,
    },

    // Function definition — desugared: true = synthesised by $_ desugaring
    // return_ann is surface-level user input, not a pass result
    Fn {
        return_ann: Option<Spanned<Annotation>>,
        params: Vec<Spanned<SurfaceParam>>,
        body: Arc<SurfaceNode>,
        desugared: bool,
    },

    // Type assertion — no resolved_type field; lives in TypeAnnotationTable keyed by NodeId
    TypeAssert {
        annotation: Spanned<Annotation>,
        expr: Arc<SurfaceNode>,
    },

    // Annotated bare word, e.g. Fn@Number
    Annotated {
        name: String,
        annotation: Spanned<Annotation>,
    },

    // Row variable / open record marker — None = unnamed (...)
    Rest(Option<String>),

    // Pattern matching
    Match {
        scrutinee: Arc<SurfaceNode>,
        arms: Vec<SurfaceMatchArm>,
    },

    // Quasiquoting
    Quote(Arc<SurfaceNode>),
    Unquote(Arc<SurfaceNode>),
    UnquoteSplice(Arc<SurfaceNode>),

    // Binding and pattern forms. Structurally valid only in specific host positions;
    // the lowering pass raises an error if these appear in other positions.
    PatternDecl {
        bindings: Vec<Arc<SurfaceNode>>,
    },
    LetDecl {
        bindings: Vec<Arc<SurfaceNode>>,
    },
    CaseArm {
        pattern: Arc<SurfaceNode>,
        body: Arc<SurfaceNode>,
    },
    TypeApp {
        func: Arc<SurfaceNode>,
        arg: Arc<SurfaceNode>,
    },

    // Placeholder `...` — evaluates to error when forced
    Placeholder,

    // Parse error node — span covers the unparseable region
    Error(Span),
}

/// A dict/list entry in a SurfaceExpression::Dict.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceEntry {
    pub key: Option<Arc<SurfaceNode>>,
    pub value: Arc<SurfaceNode>,
}

/// A named argument in a SurfaceExpression::Call.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceNamedArg {
    pub name: String,
    pub value: Arc<SurfaceNode>,
}

/// A function parameter in a SurfaceExpression::Fn.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceParam {
    pub name: String,
    pub annotation: Option<Spanned<Annotation>>,
    pub variadic: bool,
}

/// A match arm in a SurfaceExpression::Match.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceMatchArm {
    pub pattern: Spanned<Pattern>,
    pub guard: Option<Arc<SurfaceNode>>,
    pub body: Arc<SurfaceNode>,
}

/// Compile-time-only declaration forms — removed from SurfaceExpression so the
/// evaluator never needs to handle them (they are fully resolved before evaluation).
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceDeclaration {
    TypeAlias {
        params: Vec<String>,
        body: Arc<SurfaceNode>,
    },
    ClassDecl {
        name: String,
        params: Vec<String>,
        superclasses: Vec<(String, String)>,
        methods: Vec<Spanned<SurfaceEntry>>,
        determines: Vec<Arc<SurfaceNode>>,
        resolver: Option<Arc<SurfaceNode>>,
        resolver_injective: bool,
    },
    InstanceDecl {
        class_name: String,
        arms: Vec<(Arc<SurfaceNode>, Vec<Spanned<SurfaceEntry>>)>,
    },
    DefMacro {
        name: String,
        params: Arc<SurfaceNode>,
        body: Arc<SurfaceNode>,
    },
    MacroDecl {
        name: String,
        params: Arc<SurfaceNode>,
        body: Arc<SurfaceNode>,
    },
    SyntaxClass {
        name: String,
        pattern: Arc<SurfaceNode>,
        message: Option<String>,
    },
    Splice(Vec<Arc<SurfaceNode>>),
}

/// An item in a SurfaceDocument — either an expression or a compile-time declaration.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceItem {
    Expr(Arc<SurfaceNode>),
    Decl(Spanned<SurfaceDeclaration>),
}

impl SurfaceItem {
    /// Get the span of this item.
    pub fn span(&self) -> Span {
        match self {
            SurfaceItem::Expr(node) => node.span,
            SurfaceItem::Decl(decl) => decl.span,
        }
    }
}

/// A document in a SurfaceProgram — one or more items forming a scope chain.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceDocument {
    pub stage: Option<Stage>,
    pub name: Option<String>,
    pub items: Vec<SurfaceItem>,
    pub output_type: Option<Spanned<Annotation>>,
    pub expects: Option<Spanned<Annotation>>,
    pub caps: Option<Spanned<Vec<(String, Annotation)>>>,
}

impl SurfaceDocument {
    /// Iterate only the expression items (skipping declarations).
    pub fn expressions(&self) -> impl Iterator<Item = &Arc<SurfaceNode>> {
        self.items.iter().filter_map(|item| match item {
            SurfaceItem::Expr(node) => Some(node),
            SurfaceItem::Decl(_) => None,
        })
    }
}

/// A complete tinct program — one or more documents separated by ---.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceProgram {
    pub documents: Vec<Spanned<SurfaceDocument>>,
}

/// Variable resolution side table — populated by the resolver pass, keyed by NodeId.
/// Replaces VarRef.resolved: RefCell<...> in the old design.
#[derive(Debug)]
pub struct ResolutionTable(pub HashMap<NodeId, (u32, u32)>);

impl ResolutionTable {
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    pub fn get(&self, id: &NodeId) -> Option<&(u32, u32)> {
        self.0.get(id)
    }

    pub fn insert(&mut self, id: NodeId, coords: (u32, u32)) {
        self.0.insert(id, coords);
    }
}

impl Default for ResolutionTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Type annotation side table — populated by the typechecker, keyed by NodeId.
/// Replaces TypeAssert.resolved_type: RefCell<Option<Type>> in the old design.
#[derive(Debug)]
pub struct TypeAnnotationTable(pub HashMap<NodeId, Type>);

impl TypeAnnotationTable {
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    pub fn get(&self, id: &NodeId) -> Option<&Type> {
        self.0.get(id)
    }

    pub fn insert(&mut self, id: NodeId, ty: Type) {
        self.0.insert(id, ty);
    }
}

impl Default for TypeAnnotationTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Evaluator-internal expression — produced by lowering a SurfaceExpression.
/// De Bruijn coordinates are plain fields (no RefCell, no Option).
/// Never exposed to tinct code. Can be changed freely without affecting the tinct API.
///
/// Recursive positions use Arc<Spanned<CoreExpr>> directly — no CoreNode wrapper type,
/// since CoreExpr has no need for pointer-derived identity.
///
/// Serializability invariant: every field is a primitive, String, u32, Box<Type>, Vec<T>,
/// or Arc<Spanned<CoreExpr>>. No opaque Rust handles or trait objects.
#[derive(Debug, Clone)]
pub enum CoreExpr {
    // Literals
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),

    // VarRef with resolved de Bruijn coordinates
    Var {
        name: String,
        level: u32,
        slot: u32,
    },
    // Unresolvable ref (include-introduced bindings) — name-based env lookup at runtime
    FreeVar(String),

    DotAccess {
        expr: Arc<Spanned<CoreExpr>>,
        field: DotKey,
    },

    // No Pipe variant — the lowering pass rewrites Pipe to Call before evaluation.
    Sequential(Vec<Arc<Spanned<CoreExpr>>>),
    Dict(Vec<Spanned<CoreEntry>>),
    Call {
        func: Arc<Spanned<CoreExpr>>,
        args: Vec<Arc<Spanned<CoreExpr>>>,
        named_args: Vec<Spanned<CoreNamedArg>>,
        implied: bool,
    },
    Fn {
        return_ann: Option<Spanned<Annotation>>,
        params: Vec<Spanned<CoreParam>>,
        body: Arc<Spanned<CoreExpr>>,
        desugared: bool,
    },
    // Statically type-checked TypeAssert — resolved_type set from TypeAnnotationTable during lowering.
    // Runtime behavior: structural check against resolved_type at force time.
    TypeAssert {
        annotation: Spanned<Annotation>,
        expr: Arc<Spanned<CoreExpr>>,
        resolved_type: Type,
    },
    // TypeAssert for nodes absent from TypeAnnotationTable (macro-synthesized, bypassed typechecking).
    // Falls back to default if present, raises error otherwise.
    RuntimeTypeCheck {
        annotation: Spanned<Annotation>,
        expr: Arc<Spanned<CoreExpr>>,
        default: Option<Arc<Spanned<CoreExpr>>>,
    },
    Annotated {
        name: String,
        annotation: Spanned<Annotation>,
    },
    Rest(Option<String>),
    Match {
        scrutinee: Arc<Spanned<CoreExpr>>,
        arms: Vec<CoreMatchArm>,
    },
    Quote(Arc<Spanned<CoreExpr>>),
    Unquote(Arc<Spanned<CoreExpr>>),
    UnquoteSplice(Arc<Spanned<CoreExpr>>),
    PatternDecl {
        bindings: Vec<Spanned<CoreExpr>>,
    },
    LetDecl {
        bindings: Vec<Spanned<CoreExpr>>,
    },
    CaseArm {
        pattern: Arc<Spanned<CoreExpr>>,
        body: Arc<Spanned<CoreExpr>>,
    },
    TypeApp {
        func: Arc<Spanned<CoreExpr>>,
        arg: Arc<Spanned<CoreExpr>>,
    },
    Placeholder,
    Error(Span),
}

/// A dict/list entry in a CoreExpr::Dict.
#[derive(Debug, Clone)]
pub struct CoreEntry {
    pub key: Option<Arc<Spanned<CoreExpr>>>,
    pub value: Arc<Spanned<CoreExpr>>,
}

/// A named argument in a CoreExpr::Call.
#[derive(Debug, Clone)]
pub struct CoreNamedArg {
    pub name: String,
    pub value: Arc<Spanned<CoreExpr>>,
}

/// A function parameter in a CoreExpr::Fn.
#[derive(Debug, Clone)]
pub struct CoreParam {
    pub name: String,
    pub annotation: Option<Spanned<Annotation>>,
    pub variadic: bool,
}

/// A match arm in a CoreExpr::Match.
#[derive(Debug, Clone)]
pub struct CoreMatchArm {
    pub pattern: Spanned<Pattern>,
    pub guard: Option<Arc<Spanned<CoreExpr>>>,
    pub body: Arc<Spanned<CoreExpr>>,
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
            stage: None,
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
            stage: None,
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
            stage: None,
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
                stage: None,
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
                    stage: None,
                }),
                sp(Document {
                    expressions: vec![Rc::new(sp(Expr::Int(2)))],
                    name: None,
                    output_type: None,
                    expects: None,
                    caps: None,
                    stage: None,
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
