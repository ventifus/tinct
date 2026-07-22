//! AST types: `Param`, `Annotation`, `Spanned<T>`.
//! Also: `SurfaceExpression`, `SurfaceNode`, `SurfaceProgram`, `CoreExpr` (runtime-v2 types).

use crate::types::Type;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use tinct_derive::ExprConvert;

/// Standard annotation keys processed by the type system.
/// Handled by the type checker (typecheck_annot.rs, typecheck_match.rs).
pub const STANDARD_ANN_KEYS: &[&str] = &["return", "constraint", "doc", "bind", "kinds"];

/// Create a `Span` carrying the Rust source location of the call site.
///
/// Use for all values and nodes synthesized in Rust code. Error messages will show
/// `defined at src/builtins_meta.rs:1234`, making it immediately clear which Rust
/// code produced the value — every value has a real source, even Rust-generated ones.
///
/// ```rust
/// let node = Arc::new(SurfaceNode {
///     expr: SurfaceExpression::Error(rust_span!()),
///     span: rust_span!(),
/// });
/// ```
#[macro_export]
macro_rules! rust_span {
    () => {
        $crate::ast::Span::rust_source(file!(), line!())
    };
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

/// Source span (start..end). Carries file path and line/column positions.
/// Every span must carry its source file — a line/column number without a filename is meaningless.
#[derive(Debug, Clone)]
pub struct Span {
    /// File path — shared across all spans from the same file.
    pub file: Arc<str>,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    /// Optional binding name carried alongside the location for blame tracking and stack traces.
    /// Not part of span identity — two spans at the same location are equal regardless of name.
    pub name: Option<Arc<str>>,
}

// Span identity includes position AND file path — name is excluded (annotation, not identity).
// Two spans are equal only if they point to the same location in the same file.
// rust_span!() spans carry Rust file paths; user spans carry source file paths.
impl PartialEq for Span {
    fn eq(&self, other: &Self) -> bool {
        self.start_line == other.start_line
            && self.start_col == other.start_col
            && self.end_line == other.end_line
            && self.end_col == other.end_col
            && self.file == other.file
    }
}

impl Eq for Span {}

impl Hash for Span {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.start_line.hash(state);
        self.start_col.hash(state);
        self.end_line.hash(state);
        self.end_col.hash(state);
        self.file.hash(state);
    }
}

impl Span {
    /// Create a span with a mandatory file path.
    pub fn new(
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
        file: Arc<str>,
    ) -> Self {
        Self {
            file,
            start_line,
            start_col,
            end_line,
            end_col,
            name: None,
        }
    }

    /// Create a span carrying Rust source location for synthetic nodes.
    ///
    /// Use the `rust_span!()` macro instead of calling this directly — it automatically
    /// passes `file!()` and `line!()` so error messages show the Rust source location
    /// where the synthetic node was created rather than the unhelpful `1:1-1:1`.
    pub fn rust_source(file: &'static str, line: u32) -> Self {
        Self {
            file: Arc::from(file),
            start_line: line,
            start_col: 1,
            end_line: line,
            end_col: 1,
            name: None,
        }
    }

    /// Attach a binding name to this span for blame tracking.
    pub fn with_name(mut self, name: Arc<str>) -> Self {
        self.name = Some(name);
        self
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

/// A function parameter
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub annotation: Option<Spanned<Annotation>>,
    pub variadic: bool,
    /// Declaration-order index for VarAddr::Parameter(slot) lookup in EvalFrame.params.
    pub slot: u32,
    /// Type resolved by the type checker from the parameter annotation.
    /// `None` means unknown/unannotated — accept all values at runtime (gradual typing).
    pub resolved_type: Option<crate::type_def::Type>,
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
// Pattern enum deleted (T-1750) — match arm patterns are now Arc<SurfaceNode>.

/// An annotation (type shorthand or property dict)
#[derive(Debug, Clone, PartialEq)]
pub enum Annotation {
    Simple(String),
    PropertyDict(Vec<Spanned<SurfaceEntry>>),
    Annotated(Box<Annotation>, Box<Annotation>), // e.g., Seq@Int = Annotated(Simple("Seq"), Simple("Int"))
    /// Quoting sentinel — produced by the parser when it sees `@Expr` and used exclusively
    /// to mark macro parameters that receive raw quoted AST instead of evaluated values.
    ///
    /// This is a Rust-level sentinel: the parser converts `@Expr` text to `Quote` immediately,
    /// so the quoting mechanism is agnostic to the prelude's `Expr` type name. Renaming the
    /// prelude type would not break quoting as long as users still write `@Expr` in source.
    ///
    /// Note: the source-level name `Expr` is effectively reserved — any user-defined type
    /// named `Expr` will be shadowed by this sentinel in annotation position. The parser
    /// converts `@Expr` to `Quote` before type resolution, so `@Expr` can never refer to
    /// a user-defined `Expr` type.
    Quote,
}

impl Annotation {
    /// Look up a property by string key in a PropertyDict annotation.
    /// Returns a reference to the value node if found, None for Simple annotations.
    pub fn get_property(&self, key: &str) -> Option<&Arc<SurfaceNode>> {
        match self {
            Annotation::PropertyDict(entries) => entries.iter().find_map(|entry| {
                let key_node = entry.node.key.as_ref()?;
                match &key_node.expr {
                    SurfaceExpression::StringLiteral { content, .. } if content == key => {
                        Some(&entry.node.value)
                    }
                    _ => None,
                }
            }),
            Annotation::Simple(_) => None,
            Annotation::Annotated(_, inner) => inner.get_property(key),
            Annotation::Quote => None,
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
                        write!(f, "{}: {}", key, entry.node.value)?;
                    } else {
                        write!(f, "{}", entry.node.value)?;
                    }
                }
                write!(f, "]")
            }
            Annotation::Annotated(outer, inner) => write!(f, "{outer}@{inner}"),
            // Display intentionally renders as "Expr" — this is the user-visible text they
            // wrote in source (@Expr), not a semantic dependency on the prelude's Expr type.
            Annotation::Quote => write!(f, "Expr"),
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
            SurfaceExpression::U64(n) => write!(f, "{n}u"),
            SurfaceExpression::Float(n) => {
                let s = n.to_string();
                if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                    write!(f, "{s}.0")
                } else {
                    write!(f, "{s}")
                }
            }
            SurfaceExpression::StringLiteral { content: s, .. } => write!(f, "{s:?}"),
            // Emit name as-is. `%`-prefixed refs already include `%` in the name.
            // Plain identifiers and (indistinguishable) EscapedRefs both display without `$` —
            // Display is used for error messages, not source roundtripping.
            SurfaceExpression::VarRef {
                name,
                annotation: None,
                ..
            } => write!(f, "{name}"),
            SurfaceExpression::VarRef {
                name,
                annotation: Some(ann),
                ..
            } => write!(f, "{name}@{}", ann.node),
            SurfaceExpression::Placeholder(None, _) => write!(f, "..."),
            SurfaceExpression::Placeholder(Some(name), Some(ann)) => {
                write!(f, "...{name}@{}", ann.node)
            }
            SurfaceExpression::Placeholder(Some(name), None) => write!(f, "...{name}"),
            SurfaceExpression::Error(span) => write!(f, "<error at {span}>"),
            // (Annotated variant removed — handled by VarRef above)
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
                ..
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
                resolved_captures: _,
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
            SurfaceExpression::Field { expr, field, .. } => match (expr, field) {
                (Some(e), DotKey::Ident(s)) => write!(f, "{}.{s}", e),
                (Some(e), DotKey::Int(n)) => write!(f, "{}.{n}", e),
                (None, DotKey::Ident(s)) => write!(f, ".{s}"),
                (None, DotKey::Int(n)) => write!(f, ".{n}"),
            },
            SurfaceExpression::Pipe { lhs, rhs, .. } => write!(f, "{} | {}", lhs, rhs),
            SurfaceExpression::Sequential(exprs) => {
                write!(f, "(seq")?;
                for expr in exprs {
                    write!(f, " {}", expr)?;
                }
                write!(f, ")")
            }
            SurfaceExpression::TypeAssert {
                annotation, expr, ..
            } => {
                write!(f, "[@{} {}]", annotation.node, expr)
            }
            SurfaceExpression::Quote(inner) => write!(f, "[quote {}]", inner),
            SurfaceExpression::Unquote(inner) => write!(f, "[unquote {}]", inner),
            SurfaceExpression::UnquoteSplice(inner) => write!(f, "[unquote-splice {}]", inner),
            SurfaceExpression::Match { scrutinee, arms } => {
                write!(f, "[match {}", scrutinee)?;
                for arm in arms {
                    write!(f, " {}", arm.pattern.expr)?;
                    for expr in &arm.body {
                        write!(f, " {}", expr)?;
                    }
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
            SurfaceExpression::CaseArm {
                let_bindings,
                pattern,
                body,
            } => {
                write!(f, "[case {} {} {}]", let_bindings, pattern, body)
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
                            Some(a) => format!("{name}@{}", a.node),
                            None => name.clone(),
                        })
                        .collect();
                    write!(f, "[type [let {}] {}]", param_strs.join(" "), body)
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

// Display impls for Pattern and LiteralPattern deleted (T-1750).

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.file.starts_with('<') {
            write!(f, "{}:", self.file)?;
        }
        write!(
            f,
            "{}:{}-{}:{}",
            self.start_line, self.start_col, self.end_line, self.end_col
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
    /// Type guard set by the type checker. If Some, the lowerer wraps the
    /// resulting CoreExpr in a TypeAssert with this type.
    pub type_guard: TypeAnnotation,
    /// Macro expansion origin. Set by the expander on generated nodes.
    pub provenance: Provenance,
}

impl SurfaceExpression {
    /// If this is a VarRef, return its name.
    pub fn varref_name(&self) -> Option<&str> {
        if let SurfaceExpression::VarRef { name, .. } = self {
            Some(name.as_str())
        } else {
            None
        }
    }
}

/// Returns true if `ann` is an `@Expr` annotation — i.e., the `Annotation::Quote` sentinel.
///
/// The parser converts `@Expr` text to `Annotation::Quote` immediately, so this check
/// is agnostic to the prelude's `Expr` type name. Any source-level `@Expr` annotation
/// becomes the sentinel regardless of what the prelude calls its AST node type.
///
/// Used by the evaluator (eval_materialize.rs) to identify params that receive raw
/// quoted AST instead of evaluated values.
pub fn is_expr_annotation(ann: &Annotation) -> bool {
    matches!(ann, Annotation::Quote)
}

/// Default function for `Annotated.inner` when deserializing via `ExprConvert`.
/// Returns a placeholder `Arc<SurfaceNode>` wrapping a `Placeholder` expression.
/// This is only used when reconstructing an `Annotated` node from a dict repr that has no
/// `inner` field (e.g., deserialized from user-facing AST dict). Callers that need a real
/// inner VarRef must construct `Annotated` directly with the correct inner node.
pub fn annotated_inner_default() -> Arc<SurfaceNode> {
    Arc::new(SurfaceNode::new(
        SurfaceExpression::Placeholder(None, None),
        crate::rust_span!(),
    ))
}

impl SurfaceNode {
    /// Construct a SurfaceNode with empty (fresh) inline annotations.
    /// Use this instead of struct literal to avoid manually specifying `type_guard` and `provenance`.
    pub fn new(expr: SurfaceExpression, span: Span) -> Self {
        Self {
            expr,
            span,
            type_guard: TypeAnnotation::new(),
            provenance: Provenance::new(),
        }
    }

    /// Empty default — for use with struct update syntax `..SurfaceNode::default_annotations()`
    /// when you only have `expr` and `span`. The `expr` field is NOT defaulted to anything useful
    /// (uses `Placeholder`); always override it explicitly.
    #[doc(hidden)]
    pub fn default_annotations() -> Self {
        Self {
            expr: SurfaceExpression::Placeholder(None, None),
            span: crate::rust_span!(),
            type_guard: TypeAnnotation::new(),
            provenance: Provenance::new(),
        }
    }
}

/// Helper to create a fresh `SurfaceNode` from an expression and span.
/// Equivalent to `SurfaceNode::new(expr, span)` — use this macro in struct literals to keep
/// them one-liners: `Arc::new(mk_node!(expr, span))`.
#[macro_export]
macro_rules! surface_node {
    ($expr:expr, $span:expr) => {
        $crate::ast::SurfaceNode {
            expr: $expr,
            span: $span,
            type_guard: $crate::ast::TypeAnnotation::new(),
            provenance: $crate::ast::Provenance::new(),
        }
    };
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
#[derive(Debug, Clone, PartialEq, ExprConvert)]
#[expr(prefix = "Expr", helpers = "crate::surface_convert")]
pub enum SurfaceExpression {
    // Literals
    #[expr(tag = "Literal", kind = "int", inject(bare = true))]
    Int(#[expr(key = "value")] i64),
    /// Unsigned 64-bit integer (from `42u`, `0xFFu` literals)
    #[expr(tag = "Literal", kind = "u64", inject(bare = true))]
    U64(#[expr(key = "value")] u64),
    #[expr(tag = "Literal", kind = "float", inject(bare = true))]
    Float(#[expr(key = "value")] f64),
    #[expr(tag = "Literal", kind = "str", inject(bare = true))]
    StringLiteral {
        #[expr(key = "prefix")]
        prefix: String,
        #[expr(key = "delimiter")]
        delimiter: String,
        #[expr(key = "value")]
        content: String,
    },

    // Variable reference. escaped: true = $name (pin in patterns), false = bare (bind).
    // Resolution is stored inline in the `resolution` field, written once by the resolver.
    // call_dispatch is written by the type checker when this VarRef is the function in a
    // typeclass method call — the lowerer reads it to rewrite the call to the instance binding.
    #[expr(tag = "VarRef")]
    VarRef {
        #[expr(key = "name")]
        name: String,
        #[expr(skip, default = false)]
        escaped: bool,
        #[expr(skip, default_fn = "crate::ast::Resolution::new")]
        resolution: Resolution,
        #[expr(skip, default_fn = "crate::ast::CallDispatch::new")]
        call_dispatch: CallDispatch,
        /// Type/metadata annotation from `name@annotation` syntax.
        /// `x@Int` → `Simple("Int")`, `x@[type: Int  default: 0]` → `PropertyDict(...)`.
        #[expr(skip, default_fn = "Option::default")]
        annotation: Option<Spanned<Annotation>>,
    },

    // Access
    //
    // `expr: None` = leading-dot form (`.field` with no preceding expression).
    // Semantics: skip the current letrec scope frame and resolve `field` in the parent scope.
    // After lowering, produces `CoreExpr::Var` or `CoreExpr::Placeholder` — no runtime changes needed.
    // Resolution is stored inline in `resolution` (only used for the leading-dot `expr: None` case).
    // Note: the tag remains "DotAccess" for serialization/macro roundtripping
    #[expr(tag = "DotAccess")]
    Field {
        #[expr(key = "target", child_opt)]
        expr: Option<Arc<SurfaceNode>>,
        #[expr(key = "field", dot_key)]
        field: DotKey,
        #[expr(skip, default_fn = "crate::ast::Resolution::new")]
        resolution: Resolution,
        #[expr(skip, default_fn = "crate::ast::SlotAnnotation::new")]
        field_slot: SlotAnnotation,
    },

    // Pipe is surface-only — the lowering pass rewrites it to Call before evaluation.
    // Kept here so formatters and metaprogramming can distinguish pipe-form from call-form.
    #[expr(tag = "Pipe")]
    Pipe {
        #[expr(key = "lhs", child)]
        lhs: Arc<SurfaceNode>,
        #[expr(key = "rhs", child)]
        rhs: Arc<SurfaceNode>,
        /// Span of the `|` token itself, used by desugar to locate the desugared call node.
        #[expr(skip)]
        pipe_span: Option<Span>,
    },

    // Sequential let* scoping (multi-expr fn bodies, match arm bodies)
    #[expr(tag = "Sequential")]
    Sequential(#[expr(key = "exprs", child_list)] Vec<Arc<SurfaceNode>>),

    // Dict/list literal — key is None for auto-indexed (positional) entries
    #[expr(tag = "Dict")]
    Dict(#[expr(key = "entries", entry_list)] Vec<Spanned<SurfaceEntry>>),

    // Function call — implied: true = [f x y], false = [call f x y]
    #[expr(tag = "Call")]
    Call {
        #[expr(key = "fn", child)]
        func: Arc<SurfaceNode>,
        #[expr(key = "args", child_list)]
        args: Vec<Arc<SurfaceNode>>,
        #[expr(key = "named-args", named_arg_list)]
        named_args: Vec<Spanned<SurfaceNamedArg>>,
        #[expr(key = "implied")]
        implied: bool,
        /// Span of the `|` operator when this Call was produced by pipe desugaring.
        /// When set, `lower_inner` uses this span instead of the outer SurfaceNode's span,
        /// giving precise per-step location in error messages.
        #[expr(skip)]
        pipe_span: Option<Span>,
    },

    // Function definition — desugared: true = synthesised by $_ desugaring
    // return_ann is surface-level user input, not a pass result
    #[expr(tag = "Fn")]
    Fn {
        #[expr(key = "return-ann", annotation_opt)]
        return_ann: Option<Spanned<Annotation>>,
        #[expr(key = "params", param_list)]
        params: Vec<Spanned<SurfaceParam>>,
        #[expr(key = "body", child)]
        body: Arc<SurfaceNode>,
        #[expr(key = "desugared")]
        desugared: bool,
        /// Ordered list of free-variable names captured from outer scopes.
        /// Written once by the resolver after processing this Fn's body.
        /// The index of each name in this list is the ClosureCapture index assigned to
        /// VarRef nodes inside this function that reference the corresponding outer binding.
        #[expr(skip, default_fn = "crate::ast::CapturesCell::new")]
        resolved_captures: CapturesCell,
    },

    // Type assertion — resolved_type is set inline by the type checker, read by the lowerer.
    #[expr(tag = "TypeAssert")]
    TypeAssert {
        #[expr(key = "annotation", annotation, ann_span_flat)]
        annotation: Spanned<Annotation>,
        #[expr(key = "value", child, key_aliases("expr"))]
        expr: Arc<SurfaceNode>,
        #[expr(skip, default_fn = "crate::ast::TypeAnnotation::new")]
        resolved_type: TypeAnnotation,
    },

    // (Annotated variant removed — annotation is now carried on VarRef.annotation directly)

    // Pattern matching
    #[expr(tag = "Match")]
    Match {
        #[expr(key = "scrutinee", child)]
        scrutinee: Arc<SurfaceNode>,
        #[expr(key = "arms", match_arm_list)]
        arms: Vec<SurfaceMatchArm>,
    },

    // Quasiquoting
    #[expr(tag = "Quote")]
    Quote(#[expr(key = "expr", child)] Arc<SurfaceNode>),
    #[expr(tag = "Unquote")]
    Unquote(#[expr(key = "expr", child)] Arc<SurfaceNode>),
    #[expr(tag = "UnquoteSplice")]
    UnquoteSplice(#[expr(key = "expr", child)] Arc<SurfaceNode>),

    // Binding and pattern forms. Structurally valid only in specific host positions;
    // the lowering pass raises an error if these appear in other positions.
    #[expr(tag = "PatternDecl")]
    PatternDecl {
        #[expr(key = "bindings", child_list)]
        bindings: Vec<Arc<SurfaceNode>>,
    },
    #[expr(tag = "LetDecl")]
    LetDecl {
        #[expr(key = "bindings", child_list)]
        bindings: Vec<Arc<SurfaceNode>>,
    },
    #[expr(tag = "CaseArm")]
    CaseArm {
        /// The `[let ...]` node declaring which names in `pattern` are binding targets
        /// vs. pin-comparisons. Always present — the parser (T-1151) requires exactly
        /// three positional arguments: `[case [let bindings] pattern body]`.
        #[expr(key = "let-bindings", child, key_aliases("let_bindings"))]
        let_bindings: Arc<SurfaceNode>,
        #[expr(key = "pattern", child)]
        pattern: Arc<SurfaceNode>,
        #[expr(key = "body", child)]
        body: Arc<SurfaceNode>,
    },

    // Placeholder `...` — typed hole; infers as a fresh TypeVar. Can carry an optional name
    // and type annotation from `...name@Type` syntax (used in open record contexts).
    #[expr(tag = "Placeholder")]
    Placeholder(
        #[expr(key = "name", string_opt)] Option<String>,
        #[expr(skip)] Option<Spanned<Annotation>>,
    ),

    // Parse error node — span covers the unparseable region
    #[expr(tag = "AstError")]
    Error(#[expr(key = "span", span)] Span),

    // Declaration embedded inside an expression context (e.g., as a dict entry value).
    // Preserves the full SurfaceDeclaration so the type checker can register class/instance/type
    // declarations found inside dicts (Pass 0c). Evaluates to Placeholder at runtime.
    #[expr(skip)]
    Decl(Box<SurfaceDeclaration>),
}

/// A dict/list entry in a SurfaceExpression::Dict.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceEntry {
    pub key: Option<Arc<SurfaceNode>>,
    pub value: Arc<SurfaceNode>,
}

/// A named argument in a SurfaceExpression::Call.
///
/// `annotation` carries any `@[...]` annotation attached to the argument name
/// in a field declaration context, e.g. `fields@Child: [Seq TypeNode]` inside a
/// constructor bracket produces `SurfaceNamedArg { name: "fields", value: ...,
/// annotation: Some(Simple("Child")) }`.
///
/// For ordinary named arguments (`field: value`) the annotation is `None`.
/// The desugar pass (T-1053) reads this to populate `field-annotations:` in the
/// constructor's `FnAnnotation.extra`.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceNamedArg {
    pub name: String,
    pub value: Arc<SurfaceNode>,
    pub annotation: Option<Spanned<Annotation>>,
}

/// A function parameter in a SurfaceExpression::Fn.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceParam {
    pub name: String,
    pub annotation: Option<Spanned<Annotation>>,
    pub variadic: bool,
    /// Type resolved by the type checker from the parameter annotation.
    /// Set during `infer_fn_push_cont`; read by the lowerer to populate `CoreParam::resolved_type`.
    /// Clone resets to empty (cloned nodes in new scopes must be re-annotated).
    pub resolved_annotation_type: TypeAnnotation,
}

/// Flatten a dot-access chain into a qualified tag string, or return `None` if the
/// chain contains a non-identifier segment (e.g., a numeric index).
///
/// Examples:
/// - `VarRef("Result")` → `"Result"`
/// - `Field(VarRef("Result"), "Ok")` → `"Result.Ok"`
/// - `Field(Field(VarRef("Net"), "Transport"), "Tcp")` → `"Net.Transport.Tcp"`
/// - `Field(_, Int(0))` → `None` (numeric index)
///
/// Used by the parser (constructor patterns) and `typecheck_special.rs` (monad resolution).
/// Defined in `src/ast.rs` as `pub(crate)` since both callers import from here.
pub(crate) fn flatten_dot_access_to_tag(expr: &SurfaceExpression) -> Option<String> {
    match expr {
        SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
        SurfaceExpression::Field {
            expr: Some(inner),
            field: DotKey::Ident(s),
            ..
        } => Some(format!("{}.{}", flatten_dot_access_to_tag(&inner.expr)?, s)),
        SurfaceExpression::Field { expr: None, .. } => None, // leading-dot form is not a constructor name (no prefix available)
        SurfaceExpression::Field {
            field: DotKey::Int(_),
            ..
        } => None, // numeric index in a dot chain is not a constructor name
        _ => None,
    }
}

/// Flatten a dot-access chain rooted at a `SurfaceNode` to a qualified tag string.
///
/// Returns `Some(tag)` when the node is a VarRef or a chain of `Field` (dot-access) nodes
/// over an Ident key. Returns `None` for leading-dot forms, numeric indices, or other
/// expression shapes that cannot be constructor names.
///
/// This is the `SurfaceNode`-level wrapper over `flatten_dot_access_to_tag`. Prefer this
/// version when the caller already has an `Arc<SurfaceNode>`.
pub(crate) fn flatten_dot_access_to_tag_node(node: &SurfaceNode) -> Option<String> {
    flatten_dot_access_to_tag(&node.expr)
}

/// A match arm in a SurfaceExpression::Match.
///
/// `body` is a non-empty Vec of expressions. Single-body arms have `body.len() == 1`.
/// Multi-body arms have `body.len() > 1` — they work like fn multi-body: each expression
/// up to the last is an intermediate lazy scope dict; the last is the return value.
/// The lowerer wraps multi-body in `CoreExpr::Sequential` (same as fn body lowering).
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceMatchArm {
    pub pattern: Arc<SurfaceNode>,
    pub guard: Option<Arc<SurfaceNode>>,
    /// Non-empty Vec of body expressions. Single-expression arms have `body.len() == 1`.
    /// Multi-expression arms have all-but-last as intermediate scope dicts and last as result.
    pub body: Vec<Arc<SurfaceNode>>,
    /// Compile-time-resolved Matchable instance binding name for the guard expression.
    /// Resolved by the type checker after the guard's return type is inferred.
    /// When resolved, the evaluator uses this for direct dispatch instead of call_to_match.
    pub guard_matchable_binding: MatchableBinding,
}

impl SurfaceMatchArm {
    /// Returns the final (return-value) body expression. Panics if body is empty (invariant violation).
    pub fn body_expr(&self) -> &Arc<SurfaceNode> {
        self.body.last().expect("match arm body must be non-empty")
    }

    /// Returns true if this arm has more than one body expression (multi-body form).
    pub fn body_is_multi(&self) -> bool {
        self.body.len() > 1
    }
}

/// Compile-time-only declaration forms — removed from SurfaceExpression so the
/// evaluator never needs to handle them (they are fully resolved before evaluation).
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceDeclaration {
    TypeAlias {
        /// Type parameter names, each with an optional variance/class annotation.
        /// The annotation is the full `Spanned<Annotation>` from `@X` in `[let a@X b@Y c]`.
        /// `None` means no annotation was given (untyped / infer variance from body).
        params: Vec<(String, Option<Spanned<Annotation>>)>,
        body: Arc<SurfaceNode>,
    },
    ClassDecl {
        name: String,
        params: Vec<String>,
        superclasses: Vec<(String, Vec<String>)>,
        methods: Vec<Spanned<SurfaceEntry>>,
        determines: Vec<Arc<SurfaceNode>>,
        resolver: Option<Arc<SurfaceNode>>,
        resolver_injective: bool,
        /// Structural discharge rule — parsed from `structural: "closed-dict"` in class metadata.
        /// Empty string = None (normal instance resolution).
        structural: String,
    },
    InstanceDecl {
        class_name: String,
        arms: Vec<(Arc<SurfaceNode>, Vec<Spanned<SurfaceEntry>>)>,
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
    pub header: indexmap::IndexMap<String, Arc<SurfaceNode>>,
    pub items: Vec<SurfaceItem>,
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
    pub documents: Vec<Spanned<Arc<SurfaceDocument>>>,
}

/// Inline type annotation — written once by the type checker, read by the lowerer/evaluator.
/// Clone resets to empty (cloned nodes in new scopes must be re-annotated).
pub struct TypeAnnotation(std::sync::OnceLock<Option<crate::type_def::Type>>);
impl TypeAnnotation {
    pub fn new() -> Self {
        Self(std::sync::OnceLock::new())
    }
    pub fn get(&self) -> Option<&crate::type_def::Type> {
        self.0.get().and_then(|o| o.as_ref())
    }
    pub fn set(&self, val: Option<crate::type_def::Type>) {
        let _ = self.0.set(val);
    }
}
impl Clone for TypeAnnotation {
    fn clone(&self) -> Self {
        Self::new()
    }
}
impl PartialEq for TypeAnnotation {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Default for TypeAnnotation {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Debug for TypeAnnotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TypeAnnotation({:?})", self.0.get().map(|o| o.is_some()))
    }
}

/// Inline field slot — written once by the type checker for typed Field nodes.
pub struct SlotAnnotation(std::sync::OnceLock<Option<u32>>);
impl SlotAnnotation {
    pub fn new() -> Self {
        Self(std::sync::OnceLock::new())
    }
    pub fn get(&self) -> Option<u32> {
        self.0.get().copied().flatten()
    }
    pub fn set(&self, slot: u32) {
        let _ = self.0.set(Some(slot));
    }
}
impl Clone for SlotAnnotation {
    fn clone(&self) -> Self {
        Self::new()
    }
}
impl PartialEq for SlotAnnotation {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Default for SlotAnnotation {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Debug for SlotAnnotation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SlotAnnotation({:?})", self.0.get())
    }
}

/// Macro expansion provenance — set by the expander on generated nodes.
#[derive(Debug, Clone)]
pub struct MacroProvenance {
    pub macro_name: String,
    pub call_site_span: Span,
}

/// Inline call dispatch — written once by the type checker for typeclass method VarRef nodes.
///
/// Stores the resolved `VarAddr` for the concrete instance binding.  The type checker sets
/// this after argument-type unification determines the concrete instance; the lowerer reads
/// it and emits a `CoreExpr::Var` with that address directly, without any de Bruijn conversion.
///
/// Written at most once by the type checker (after argument-type unification determines the
/// concrete instance). Read once by the lowerer when emitting the Call's function sub-expression.
/// Interior-mutable via `OnceLock` so the type checker can set it through a shared reference
/// to the `Arc<SurfaceNode>` that owns the VarRef.
pub struct CallDispatch(std::sync::OnceLock<VarAddr>);
impl CallDispatch {
    pub fn new() -> Self {
        Self(std::sync::OnceLock::new())
    }
    /// Returns the resolved `VarAddr` if set, or `None` if not yet dispatched.
    pub fn get(&self) -> Option<&VarAddr> {
        self.0.get()
    }
    /// Set the resolved `VarAddr`.  Silently ignores a second call (OnceLock
    /// semantics): the first write wins.  Call sites must ensure the write happens at most
    /// once per VarRef (guaranteed because type-checking is a single forward pass).
    pub fn set(&self, addr: VarAddr) {
        let _ = self.0.set(addr);
    }
}
impl Clone for CallDispatch {
    fn clone(&self) -> Self {
        Self::new()
    }
}
impl PartialEq for CallDispatch {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Default for CallDispatch {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Debug for CallDispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CallDispatch({:?})", self.0.get())
    }
}

pub struct Provenance(std::sync::OnceLock<Option<MacroProvenance>>);
impl Provenance {
    pub fn new() -> Self {
        Self(std::sync::OnceLock::new())
    }
    pub fn get(&self) -> Option<&MacroProvenance> {
        self.0.get().and_then(|o| o.as_ref())
    }
    pub fn set(&self, p: MacroProvenance) {
        let _ = self.0.set(Some(p));
    }
}
impl Clone for Provenance {
    fn clone(&self) -> Self {
        Self::new()
    }
}
impl PartialEq for Provenance {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Default for Provenance {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Debug for Provenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Provenance({:?})",
            self.0.get().and_then(|p| p.as_ref()).map(|p| &p.macro_name)
        )
    }
}

/// Inline VarAddr for a VarRef or leading-dot Field node.
/// Written once by the resolver; read by the lowerer.
/// Clone resets to empty — cloned nodes are in new scopes and must be re-resolved.
pub struct Resolution(std::sync::OnceLock<Option<VarAddr>>);

impl Resolution {
    pub fn new() -> Self {
        Self(std::sync::OnceLock::new())
    }
    /// `Some(Some(addr))` = resolved; `Some(None)` = unresolvable; `None` = not yet resolved.
    pub fn get(&self) -> Option<Option<&VarAddr>> {
        self.0.get().map(|o| o.as_ref())
    }
    /// Called by the resolver exactly once per node instance.
    pub fn set(&self, val: Option<VarAddr>) {
        let _ = self.0.set(val);
    }
}
impl Clone for Resolution {
    fn clone(&self) -> Self {
        Self::new()
    }
}
impl PartialEq for Resolution {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Default for Resolution {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Debug for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Resolution({:?})", self.0.get().map(|o| o.is_some()))
    }
}

/// Table mapping each VarRef's NodeId to its resolved VarAddr.
/// Produced by the resolver pass; consumed by the lowerer and type checker.
pub type ResolutionTable = std::collections::HashMap<NodeId, VarAddr>;

/// Capture list for a function node — the ordered list of (name, original_addr) pairs
/// captured from outer scopes, in first-occurrence order as seen during resolver traversal.
/// `original_addr` is the VarAddr the captured variable held in the enclosing frame
/// (LetrecGroupMember, ClosureCapture, or Parameter), BEFORE the resolver converts it to
/// ClosureCapture for references inside the function body.
/// Written once by the resolver after processing each Fn body; read by the lowerer.
/// Clone resets to empty — cloned Fn nodes are in new scopes and must be re-resolved.
pub struct CapturesCell(std::sync::OnceLock<Arc<Vec<(String, VarAddr)>>>);

impl CapturesCell {
    pub fn new() -> Self {
        Self(std::sync::OnceLock::new())
    }
    /// Returns the capture list if the resolver has set it, or `None` if not yet resolved.
    pub fn get(&self) -> Option<&Arc<Vec<(String, VarAddr)>>> {
        self.0.get()
    }
    /// Called by the resolver exactly once per Fn node instance.
    pub fn set(&self, captures: Arc<Vec<(String, VarAddr)>>) {
        let _ = self.0.set(captures);
    }
}
impl Clone for CapturesCell {
    fn clone(&self) -> Self {
        Self::new()
    }
}
impl PartialEq for CapturesCell {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Default for CapturesCell {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Debug for CapturesCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CapturesCell({:?})", self.0.get().map(|v| v.len()))
    }
}

/// Table mapping each TypeAssert SurfaceNode's NodeId to its resolved Type.
/// Produced by the type checker; consumed by the lowerer to generate CoreExpr::TypeAssert.
pub type TypeAnnotationTable = std::collections::HashMap<NodeId, crate::types::Type>;

/// Inline annotation for a predicate pattern's resolved Matchable `to-match` instance
/// binding name. Written once by the type checker during match arm elaboration; read by
/// the lowerer to carry the binding name into the match arm.
///
/// Clone resets to empty (same semantics as other OnceLock annotations — cloned patterns
/// in new scopes need fresh resolution).
pub struct MatchableBinding(std::sync::OnceLock<Option<String>>);
impl MatchableBinding {
    pub fn new() -> Self {
        Self(std::sync::OnceLock::new())
    }
    pub fn get(&self) -> Option<&String> {
        self.0.get().and_then(|o| o.as_ref())
    }
    pub fn set(&self, val: Option<String>) {
        let _ = self.0.set(val);
    }
}
impl Clone for MatchableBinding {
    fn clone(&self) -> Self {
        // Preserve the resolved binding through clones — unlike Resolution/TypeAnnotation,
        // MatchableBinding is scope-independent (the instance binding name is global).
        match self.0.get() {
            Some(val) => {
                let new = Self::new();
                let _ = new.0.set(val.clone());
                new
            }
            None => Self::new(),
        }
    }
}
impl PartialEq for MatchableBinding {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Default for MatchableBinding {
    fn default() -> Self {
        Self::new()
    }
}
impl std::fmt::Debug for MatchableBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MatchableBinding({:?})", self.0.get())
    }
}

/// Variable addressing after closure conversion.
/// Replaces (level, slot) de Bruijn coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VarAddr {
    /// Index into EvalFrame.group (current letrec group thunks)
    LetrecGroupMember(u32),
    /// Index into EvalFrame.closure_env (fn-captured outer scope thunks).
    /// Emitted exclusively for fn captures: a VarRef inside a fn body that refers to a
    /// name outside the fn boundary. `i` is the index into the fn's capture list
    /// (`resolved_captures`). At fn-definition time, the evaluator walks the capture list
    /// and copies thunks from the enclosing EvalFrame into `closure_env`.
    ClosureCapture(u32),
    /// Reference to slot `slot` reached by traversing `hops` outer-frame links:
    /// `frame.outer^hops.group[slot]`.
    ///
    /// Emitted for cross-dict references that are NOT inside a fn boundary: a VarRef in
    /// an inner dict body that refers to a name defined in an enclosing dict scope.
    /// At runtime, resolved by walking `hops` `frame.outer` links and then indexing
    /// `group[slot]`.
    ///
    /// `hops = count(ScopeKind::Dict scopes strictly above match_depth)` — each Dict scope
    /// above the reference site is a real eval_dict_core frame boundary requiring one hop.
    ///
    /// For fn captures: `hops = 1 + count(fn_scope_boundaries strictly between match_depth
    /// and fn_boundary)` — the base hop crosses the current fn's letrec scope, and each
    /// outer-fn boundary crossed adds one more hop via the fn_outer chain.
    OuterGroupRef(u32, u32),
    /// Index into EvalFrame.params (function call arguments)
    Parameter(u32),
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
    /// Unsigned 64-bit integer (from `42u`, `0xFFu` literals)
    U64(u64),
    Float(f64),
    Str(String),

    // VarRef with closure-converted addressing
    Var {
        name: String,
        addr: VarAddr,
        /// Annotation from `name@annotation` syntax. Simple("T") for bare type names,
        /// PropertyDict for user-written @[type: T  ...] forms.
        /// None for plain variable references.
        annotation: Option<Spanned<Annotation>>,
    },

    /// First-class variant constructor. Produced by lower.rs for type declarations.
    /// Encoded directly in CoreExpr — no runtime function call needed.
    Variant {
        tag: String,
        payload: Option<Arc<Spanned<CoreExpr>>>,
    },

    UnitVariant {
        tycon: String,
        ctor: String,
    },

    // No Pipe variant — the lowering pass rewrites Pipe to Call before evaluation.
    // No Field/Slot variants — dot-access is desugared to Call(field-get/slot-get, [key, target])
    // by the lowerer. See lower.rs and builtins_core.rs for FIELD_GET_ROOT_SLOT / SLOT_GET_ROOT_SLOT.
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
        /// Ordered list of (name, original_addr) pairs for variables captured from outer scopes.
        /// `original_addr` is the VarAddr the binding held in the enclosing EvalFrame
        /// (LetrecGroupMember(i), ClosureCapture(i), or Parameter(i)) — i.e. the address BEFORE
        /// the resolver converted it to ClosureCapture for references inside this function.
        /// At function-definition time the evaluator uses these original addresses to look up
        /// each captured thunk in the current EvalFrame and build `closure_env`.
        /// Written by the lowerer from `SurfaceExpression::Fn::resolved_captures`.
        captures: std::sync::Arc<Vec<(String, VarAddr)>>,
    },
    // Statically type-checked TypeAssert — resolved_type read from the inline TypeAnnotation
    // on the SurfaceExpression::TypeAssert node during lowering.
    // Runtime behavior: structural check against resolved_type at force time.
    TypeAssert {
        annotation: Spanned<Annotation>,
        expr: Arc<Spanned<CoreExpr>>,
        resolved_type: Type,
        /// Pipeline blame for `--- expects: @Type` contract assertions.
        /// Set when a document's `--- expects:` annotation is resolved for pipeline type validation.
        /// None for all other TypeAssert sites (user-written `[@Type expr]` annotations).
        pipeline_blame: Option<crate::error::PipelineBlame>,
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
        /// The lowered `[let ...]` node declaring binding targets. Always present —
        /// the parser (T-1151) requires exactly three positional arguments:
        /// `[case [let bindings] pattern body]`.
        let_bindings: Arc<Spanned<CoreExpr>>,
        pattern: Arc<Spanned<CoreExpr>>,
        body: Arc<Spanned<CoreExpr>>,
    },
    Placeholder,
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
    /// Declaration-order index for VarAddr::Parameter(slot) lookup in EvalFrame.params.
    pub slot: u32,
    /// Type resolved by the type checker from the parameter annotation.
    /// `None` means unknown/unannotated — accept all values at runtime (gradual typing).
    pub resolved_type: Option<crate::type_def::Type>,
}

/// A match arm in a CoreExpr::Match.
#[derive(Debug, Clone)]
pub struct CoreMatchArm {
    pub pattern: Arc<SurfaceNode>,
    pub guard: Option<Arc<Spanned<CoreExpr>>>,
    pub body: Arc<Spanned<CoreExpr>>,
    /// Pre-resolved Matchable instance binding name for the guard's return type.
    /// Set by the type checker on the SurfaceMatchArm and carried through lowering.
    pub guard_matchable_binding: MatchableBinding,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::sp;

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
        // Annotation keys from the parser are always SurfaceExpression::StringLiteral (bare words).
        let zero_span = Span {
            file: rust_span!().file,
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 0,
            name: None,
        };
        let mk_node = |expr: SurfaceExpression| -> Arc<SurfaceNode> {
            Arc::new(SurfaceNode {
                expr,
                span: zero_span.clone(),
                type_guard: TypeAnnotation::new(),
                provenance: Provenance::new(),
            })
        };
        let ann = Annotation::PropertyDict(vec![
            sp(SurfaceEntry {
                key: Some(mk_node(SurfaceExpression::StringLiteral {
                    prefix: String::new(),
                    delimiter: "\"".to_string(),
                    content: "type".into(),
                })),
                value: mk_node(SurfaceExpression::StringLiteral {
                    prefix: String::new(),
                    delimiter: "\"".to_string(),
                    content: "Number".into(),
                }),
            }),
            sp(SurfaceEntry {
                key: Some(mk_node(SurfaceExpression::StringLiteral {
                    prefix: String::new(),
                    delimiter: "\"".to_string(),
                    content: "default".into(),
                })),
                value: mk_node(SurfaceExpression::Int(42)),
            }),
        ]);
        assert_eq!(format!("{ann}"), "[\"type\": \"Number\"  \"default\": 42]");
    }

    #[test]
    fn test_display_annotation_quote() {
        // Annotation::Quote must render as "Expr" — this is the user-visible source text.
        let ann = Annotation::Quote;
        assert_eq!(format!("{ann}"), "Expr");
    }

    #[test]
    fn test_is_expr_annotation_quote_returns_true() {
        // The quoting sentinel variant must be recognized as the expr annotation.
        assert!(is_expr_annotation(&Annotation::Quote));
    }

    #[test]
    fn test_is_expr_annotation_simple_expr_returns_false() {
        // Simple("Expr") must NOT trigger quoting — the parser converts @Expr to Quote
        // immediately. If it ever produces Simple("Expr") instead, quoting silently breaks.
        assert!(!is_expr_annotation(&Annotation::Simple("Expr".to_string())));
    }

    // ── Span PartialEq and Hash identity tests ────────────────────────────────
    //
    // T-1771 restructured Span: `name` is explicitly excluded from span identity.
    // Two spans at the same file/line/col are equal even if their names differ.
    // Two spans at different line/col are not equal even if their names match.

    #[test]
    fn test_span_partialeq_excludes_name() {
        let file: Arc<str> = Arc::from("test.llt");
        let span_a = Span {
            file: Arc::clone(&file),
            start_line: 1,
            start_col: 5,
            end_line: 1,
            end_col: 10,
            name: Some(Arc::from("foo")),
        };
        let span_b = Span {
            file: Arc::clone(&file),
            start_line: 1,
            start_col: 5,
            end_line: 1,
            end_col: 10,
            name: Some(Arc::from("bar")), // different name
        };
        // Same file/line/col — must be equal regardless of name.
        assert_eq!(
            span_a, span_b,
            "Spans with identical file/line/col must be equal even if name differs"
        );
    }

    #[test]
    fn test_span_hash_excludes_name() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let file: Arc<str> = Arc::from("test.llt");
        let hash_span = |name: Option<&str>| {
            let s = Span {
                file: Arc::clone(&file),
                start_line: 2,
                start_col: 0,
                end_line: 2,
                end_col: 4,
                name: name.map(Arc::from),
            };
            let mut hasher = DefaultHasher::new();
            s.hash(&mut hasher);
            hasher.finish()
        };

        // Both spans have the same file/line/col but different names — hashes must match.
        assert_eq!(
            hash_span(Some("alpha")),
            hash_span(Some("beta")),
            "Spans with identical file/line/col must hash the same regardless of name"
        );
        assert_eq!(
            hash_span(Some("x")),
            hash_span(None),
            "Span with name and Span without name at same location must hash the same"
        );
    }

    #[test]
    fn test_span_partialeq_different_location_not_equal() {
        let file: Arc<str> = Arc::from("test.llt");
        let span_a = Span {
            file: Arc::clone(&file),
            start_line: 3,
            start_col: 1,
            end_line: 3,
            end_col: 5,
            name: Some(Arc::from("same-name")),
        };
        let span_b = Span {
            file: Arc::clone(&file),
            start_line: 4, // different line
            start_col: 1,
            end_line: 4,
            end_col: 5,
            name: Some(Arc::from("same-name")),
        };
        // Different line — must not be equal even if name is identical.
        assert_ne!(
            span_a, span_b,
            "Spans at different lines must not be equal even if name matches"
        );
    }
}
