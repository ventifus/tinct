//! AST types: `Param`, `Annotation`, `Spanned<T>`, `Pattern`.
//! Also: `SurfaceExpression`, `SurfaceNode`, `SurfaceProgram`, `CoreExpr` (runtime-v2 types).
//! Note: `File`, `Document`, `Expr`, `Entry`, `NamedArg`, `MatchArm` deleted in sprint rv2-delete-old-ast (2026-05-24).

use crate::types::Type;
use std::collections::HashMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Source file with path and content, shared across all spans from the same file.
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub path: Arc<str>,
    pub content: Arc<str>,
}

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
#[derive(Debug, Clone)]
pub struct Span {
    pub start: Position,
    pub end: Position,
    pub file: Option<Arc<SourceFile>>,
}

// Manual PartialEq, Eq, and Hash implementations that ignore the `file` field.
// This preserves existing semantics: two spans at the same location are equal
// regardless of what file they came from.
impl PartialEq for Span {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start && self.end == other.end
    }
}

impl Eq for Span {}

impl Hash for Span {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.start.hash(state);
        self.end.hash(state);
    }
}

impl Span {
    pub fn new(start: Position, end: Position) -> Self {
        Self {
            start,
            end,
            file: None,
        }
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
        Self {
            start: pos,
            end: pos,
            file: None,
        }
    }

    /// Returns true if this span is the synthetic origin span (all zeros / line 1, col 1).
    ///
    /// Used to guard against false-positive boundary guard matches: synthetic `CoreExpr`
    /// nodes created by macro expansion or internal synthesis all share `Span::origin()`,
    /// so keying `boundary_guards` by span would collide across all of them.
    /// `maybe_wrap_guard` skips guarding for origin spans to prevent applying the wrong
    /// type guard to an unrelated synthetic node.
    pub fn is_origin(&self) -> bool {
        *self == Self::origin()
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

// File, Document, Expr, Entry, NamedArg, and MatchArm deleted.
// Deleted in sprint rv2-delete-old-ast (2026-05-24).
// All callers now use SurfaceProgram/SurfaceDocument/SurfaceExpression/CoreExpr.

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

// MatchArm deleted (used Expr which is now deleted). Replaced by SurfaceMatchArm.

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
    /// `[None]` (bracket form) matches `Variant { tag: "None", payload: None }` via Constructor { binding: None }
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
    PropertyDict(Vec<Spanned<SurfaceEntry>>),
    Annotated(String, Box<Annotation>), // e.g., Seq@Int = Annotated("Seq", Simple("Int"))
}

impl Annotation {
    /// Look up a property by string key in a PropertyDict annotation.
    /// Returns a reference to the value node if found, None for Simple annotations.
    pub fn get_property(&self, key: &str) -> Option<&Arc<SurfaceNode>> {
        match self {
            Annotation::PropertyDict(entries) => entries.iter().find_map(|entry| {
                let key_node = entry.node.key.as_ref()?;
                match &key_node.expr {
                    SurfaceExpression::Str(name) if name == key => Some(&entry.node.value),
                    _ => None,
                }
            }),
            Annotation::Simple(_) => None,
            Annotation::Annotated(_, _) => None,
        }
    }
}

// Display impls for File, Document, Expr deleted (types deleted in sprint rv2-delete-old-ast 2026-05-24).

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
                        write!(f, "{}: {}", key, entry.node.value)?;
                    } else {
                        write!(f, "{}", entry.node.value)?;
                    }
                }
                write!(f, "]")
            }
            Annotation::Annotated(name, inner) => write!(f, "{name}@{inner}"),
        }
    }
}

impl fmt::Display for SurfaceNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.expr)
    }
}

impl fmt::Display for SurfaceExpression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SurfaceExpression::Int(n) => write!(f, "{n}"),
            SurfaceExpression::Float(n) => {
                let s = n.to_string();
                if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                    write!(f, "{s}.0")
                } else {
                    write!(f, "{s}")
                }
            }
            SurfaceExpression::Bool(b) => write!(f, "{b}"),
            SurfaceExpression::Str(s) => write!(f, "{s:?}"),
            // Emit name as-is. `%`-prefixed refs already include `%` in the name.
            // Plain identifiers and (indistinguishable) EscapedRefs both display without `$` —
            // Display is used for error messages, not source roundtripping.
            SurfaceExpression::VarRef { name, .. } => write!(f, "{name}"),
            SurfaceExpression::Placeholder => write!(f, "..."),
            SurfaceExpression::Rest(None) => write!(f, "..."),
            SurfaceExpression::Rest(Some(name)) => write!(f, "...{name}"),
            SurfaceExpression::Error(span) => write!(f, "<error at {span}>"),
            SurfaceExpression::Annotated { name, annotation } => {
                write!(f, "{name}@{}", annotation.node)
            }
            SurfaceExpression::Dict(entries) => {
                write!(f, "[")?;
                for (i, entry) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, "  ")?;
                    }
                    if let Some(key) = &entry.node.key {
                        write!(f, "{}: {}", key, entry.node.value)?;
                    } else {
                        write!(f, "{}", entry.node.value)?;
                    }
                }
                write!(f, "]")
            }
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                implied,
            } => {
                if *implied {
                    write!(f, "[{}", func)?;
                } else {
                    write!(f, "[call {}", func)?;
                }
                for arg in args {
                    write!(f, " {}", arg)?;
                }
                for na in named_args {
                    write!(f, " {}: {}", na.node.name, na.node.value)?;
                }
                write!(f, "]")
            }
            SurfaceExpression::Fn {
                return_ann,
                params,
                body,
                desugared: _,
            } => {
                write!(f, "[fn")?;
                if let Some(ann) = return_ann {
                    write!(f, "@{}", ann.node)?;
                }
                write!(f, " [let")?;
                for p in params.iter() {
                    write!(f, " ")?;
                    if p.node.variadic {
                        write!(f, "...")?;
                    }
                    write!(f, "{}", p.node.name)?;
                    if let Some(ann) = &p.node.annotation {
                        write!(f, "@{}", ann.node)?;
                    }
                }
                write!(f, "] {}]", body)
            }
            SurfaceExpression::DotAccess { expr, field } => match field {
                DotKey::Ident(s) => write!(f, "{}.{s}", expr),
                DotKey::Int(n) => write!(f, "{}.{n}", expr),
            },
            SurfaceExpression::Pipe { lhs, rhs } => write!(f, "{} | {}", lhs, rhs),
            SurfaceExpression::Sequential(exprs) => {
                write!(f, "(seq")?;
                for expr in exprs {
                    write!(f, " {}", expr)?;
                }
                write!(f, ")")
            }
            SurfaceExpression::TypeAssert { annotation, expr } => {
                write!(f, "[@{} {}]", annotation.node, expr)
            }
            SurfaceExpression::Quote(inner) => write!(f, "[quote {}]", inner),
            SurfaceExpression::Unquote(inner) => write!(f, "[unquote {}]", inner),
            SurfaceExpression::UnquoteSplice(inner) => write!(f, "[unquote-splice {}]", inner),
            SurfaceExpression::Match { scrutinee, arms } => {
                write!(f, "[match {}", scrutinee)?;
                for arm in arms {
                    write!(f, " {} {}", arm.pattern.node, arm.body)?;
                }
                write!(f, "]")
            }
            SurfaceExpression::LetDecl { bindings } => {
                write!(f, "[let")?;
                for binding in bindings {
                    write!(f, " {}", binding)?;
                }
                write!(f, "]")
            }
            SurfaceExpression::PatternDecl { bindings } => {
                write!(f, "[pattern")?;
                for binding in bindings {
                    write!(f, " {}", binding)?;
                }
                write!(f, "]")
            }
            SurfaceExpression::CaseArm { pattern, body } => {
                write!(f, "[case {} {}]", pattern, body)
            }
            SurfaceExpression::Decl(decl) => {
                // Delegate to SurfaceDeclaration Display.
                write!(f, "{}", decl)
            }
        }
    }
}

impl fmt::Display for SurfaceDeclaration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SurfaceDeclaration::TypeAlias { params, body } => {
                if params.is_empty() {
                    write!(f, "[type {}]", body)
                } else {
                    let param_strs: Vec<String> = params
                        .iter()
                        .map(|(name, ann)| match ann {
                            Some(a) => format!("{name}@{a}"),
                            None => name.clone(),
                        })
                        .collect();
                    write!(f, "[type [{}] {}]", param_strs.join(" "), body)
                }
            }
            SurfaceDeclaration::ClassDecl {
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
                        write!(f, " {}: {}", key, entry.node.value)?;
                    }
                }
                write!(f, "]")
            }
            SurfaceDeclaration::InstanceDecl { class_name, arms } => {
                write!(f, "[instance {class_name}")?;
                for (pattern, methods) in arms {
                    write!(f, " {}", pattern)?;
                    write!(f, ":")?;
                    for entry in methods {
                        if let Some(key) = &entry.node.key {
                            write!(f, " {}: {}", key, entry.node.value)?;
                        }
                    }
                }
                write!(f, "]")
            }
            SurfaceDeclaration::MacroDecl { name, params, body } => {
                write!(f, "[macro {} {} {}]", name, params, body)
            }
            SurfaceDeclaration::SyntaxClass {
                name,
                pattern,
                message,
            } => {
                write!(f, "[syntax-class {} pattern: {}", name, pattern)?;
                if let Some(msg) = message {
                    write!(f, " message: {:?}", msg)?;
                }
                write!(f, "]")
            }
            SurfaceDeclaration::Splice(forms) => {
                write!(f, "[splice")?;
                for form in forms {
                    write!(f, " {}", form)?;
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

// impl Expr deleted (Expr deleted). var_ref/escaped_ref constructors were test-only.

// ============================================================================
// runtime-v2 AST types (Sprint 1, Part A)
// ============================================================================
//
// SurfaceExpression / SurfaceNode / SurfaceProgram — immutable, Send+Sync, Arc-recursive.
// CoreExpr — evaluator-internal representation with de Bruijn coordinates.

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

    // Placeholder `...` — evaluates to error when forced
    Placeholder,

    // Parse error node — span covers the unparseable region
    Error(Span),

    // Declaration embedded inside an expression context (e.g., as a dict entry value).
    // Preserves the full SurfaceDeclaration so the type checker can register class/instance/type
    // declarations found inside dicts (Pass 0c). Evaluates to Placeholder at runtime.
    Decl(Box<SurfaceDeclaration>),
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

/// Flatten a dot-access chain into a qualified tag string, or return `None` if the
/// chain contains a non-identifier segment (e.g., a numeric index).
///
/// Examples:
/// - `VarRef("Result")` → `"Result"`
/// - `DotAccess(VarRef("Result"), "Ok")` → `"Result.Ok"`
/// - `DotAccess(DotAccess(VarRef("Net"), "Transport"), "Tcp")` → `"Net.Transport.Tcp"`
/// - `DotAccess(_, Int(0))` → `None` (numeric index)
///
/// Used by the parser (constructor patterns) and `typecheck_special.rs` (monad resolution).
/// Defined in `src/ast.rs` as `pub(crate)` since both callers import from here.
pub(crate) fn flatten_dot_access_to_tag(expr: &SurfaceExpression) -> Option<String> {
    match expr {
        SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
        SurfaceExpression::DotAccess {
            expr,
            field: DotKey::Ident(s),
        } => Some(format!("{}.{}", flatten_dot_access_to_tag(&expr.expr)?, s)),
        SurfaceExpression::DotAccess {
            field: DotKey::Int(_),
            ..
        } => None, // numeric index in a dot chain is not a constructor name
        _ => None,
    }
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
        /// Type parameter names, each with an optional variance/class annotation name.
        /// The annotation is the bare name from `@X` in `[let a@X b@Y c]`, e.g. `"Covariant"`.
        /// `None` means no annotation was given (untyped / infer variance from body).
        params: Vec<(String, Option<String>)>,
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
            SurfaceItem::Expr(node) => node.span.clone(),
            SurfaceItem::Decl(decl) => decl.span.clone(),
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
    pub uses: Option<Spanned<Vec<Spanned<String>>>>,
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
#[derive(Debug, Clone)]
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

    pub fn drain(&mut self) -> std::collections::hash_map::Drain<'_, NodeId, Type> {
        self.0.drain()
    }
}

impl Default for TypeAnnotationTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Static empty ResolutionTable singleton.
/// Use this instead of `&ResolutionTable::new()` to avoid heap allocations.
static EMPTY_RESOLUTION_TABLE: std::sync::OnceLock<ResolutionTable> = std::sync::OnceLock::new();

/// Returns a static empty ResolutionTable.
/// Use this instead of `&ResolutionTable::new()` to avoid heap allocations.
pub fn empty_resolution_table() -> &'static ResolutionTable {
    EMPTY_RESOLUTION_TABLE.get_or_init(ResolutionTable::new)
}

/// Static empty TypeAnnotationTable singleton.
/// Use this instead of `&TypeAnnotationTable::new()` to avoid heap allocations.
static EMPTY_TYPE_ANNOTATION_TABLE: std::sync::OnceLock<TypeAnnotationTable> =
    std::sync::OnceLock::new();

/// Returns a static empty TypeAnnotationTable.
/// Use this instead of `&TypeAnnotationTable::new()` to avoid heap allocations.
pub fn empty_type_annotation_table() -> &'static TypeAnnotationTable {
    EMPTY_TYPE_ANNOTATION_TABLE.get_or_init(TypeAnnotationTable::new)
}

/// Static empty ResolutionTable singleton (Arc-wrapped version).
/// Use this instead of `Arc::new(ResolutionTable::new())` to avoid heap allocations.
static EMPTY_RESOLUTION_TABLE_ARC: std::sync::OnceLock<std::sync::Arc<ResolutionTable>> =
    std::sync::OnceLock::new();

/// Returns an Arc-wrapped static empty ResolutionTable.
/// Use this instead of `Arc::new(ResolutionTable::new())` to avoid heap allocations.
pub fn empty_resolution_table_arc() -> std::sync::Arc<ResolutionTable> {
    std::sync::Arc::clone(
        EMPTY_RESOLUTION_TABLE_ARC.get_or_init(|| std::sync::Arc::new(ResolutionTable::new())),
    )
}

/// Static empty TypeAnnotationTable singleton (Arc-wrapped version).
/// Use this instead of `Arc::new(TypeAnnotationTable::new())` to avoid heap allocations.
static EMPTY_TYPE_ANNOTATION_TABLE_ARC: std::sync::OnceLock<std::sync::Arc<TypeAnnotationTable>> =
    std::sync::OnceLock::new();

/// Returns an Arc-wrapped static empty TypeAnnotationTable.
/// Use this instead of `Arc::new(TypeAnnotationTable::new())` to avoid heap allocations.
pub fn empty_type_annotation_table_arc() -> std::sync::Arc<TypeAnnotationTable> {
    std::sync::Arc::clone(
        EMPTY_TYPE_ANNOTATION_TABLE_ARC
            .get_or_init(|| std::sync::Arc::new(TypeAnnotationTable::new())),
    )
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
        /// Pipeline blame for `--- expects: @Type` contract assertions.
        /// Set by `wrap_with_nominal_validation` when a document has an `expects:` annotation.
        /// None for all other TypeAssert sites (user-written `[@Type expr]` annotations).
        pipeline_blame: Option<crate::error::PipelineBlame>,
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
    /// Type declaration in dict value position (B-296 evaluator-level constructor injection).
    /// Carries only unit constructor names extracted from the TypeAlias body.
    /// Field constructors continue to use desugar-pass injection for now.
    TypeDecl {
        unit_constructors: Vec<String>,
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

// Tests for Expr, File, Document display impls deleted (types deleted in sprint rv2-delete-old-ast 2026-05-24).
// Annotation display tests kept below.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::sp;

    // Tests for Expr display (TypeAssert, Annotated, Call, Fn, Dict) deleted —
    // Expr type deleted in sprint rv2-delete-old-ast (2026-05-24).

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
        // Annotation keys from the parser are always SurfaceExpression::Str (bare words).
        let zero_span = Span {
            start: Position {
                offset: 0,
                line: 0,
                column: 0,
            },
            end: Position {
                offset: 0,
                line: 0,
                column: 0,
            },
            file: None,
        };
        let mk_node = |expr: SurfaceExpression| -> Arc<SurfaceNode> {
            Arc::new(SurfaceNode {
                expr,
                span: zero_span.clone(),
            })
        };
        let ann = Annotation::PropertyDict(vec![
            sp(SurfaceEntry {
                key: Some(mk_node(SurfaceExpression::Str("type".into()))),
                value: mk_node(SurfaceExpression::Str("Number".into())),
            }),
            sp(SurfaceEntry {
                key: Some(mk_node(SurfaceExpression::Str("default".into()))),
                value: mk_node(SurfaceExpression::Int(42)),
            }),
        ]);
        assert_eq!(format!("{ann}"), "[\"type\": \"Number\"  \"default\": 42]");
    }

    // Tests for Document, File, Expr::Rest, Expr::Error display deleted —
    // those types deleted in sprint rv2-delete-old-ast (2026-05-24).
}
