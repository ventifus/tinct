//! AST types: `Param`, `Annotation`, `Spanned<T>`, `Pattern`.
//! Also: `SurfaceExpression`, `SurfaceNode`, `SurfaceProgram`, `CoreExpr` (runtime-v2 types).

use crate::types::Type;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use tinct_derive::ExprConvert;

/// Standard annotation keys processed by the type system.
/// Handled by the type checker (typecheck_annot.rs, typecheck_match.rs).
/// Note: `"doc"` IS evaluated at runtime by eval_core.rs (extract_fn_annotation_extra),
/// because doc strings are string-valued expressions that evaluate safely.
/// The keys that must be EXCLUDED from runtime evaluation are in
/// `ANNOTATION_EVAL_EXCLUDED_KEYS` below — do not duplicate either list.
pub const STANDARD_ANN_KEYS: &[&str] = &["return", "constraint", "doc", "bind", "kinds"];

/// Annotation keys excluded from runtime evaluation in `extract_fn_annotation_extra`.
///
/// `"return"`, `"constraint"`, `"bind"`, `"kinds"` contain type expressions whose
/// VarRefs may have no runtime resolution slots (e.g. type variables `a`, `b`), so
/// evaluating them at function-definition time would produce lowering diagnostics.
///
/// `"doc"` is intentionally NOT in this list — it is a string-valued expression
/// (including triple-quoted strings desugared to `[unindent "..."]`) that evaluates
/// correctly in the definition-site environment and is exposed via `annotation-of`.
pub(crate) const ANNOTATION_EVAL_EXCLUDED_KEYS: &[&str] =
    &["return", "constraint", "bind", "kinds"];

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

/// Source span (start..end). Every span must carry its source file —
/// a line/column number without a filename is meaningless.
#[derive(Debug, Clone)]
pub struct Span {
    pub start: Position,
    pub end: Position,
    pub file: Arc<SourceFile>,
}

// Span identity includes position AND file path.
// Two spans are equal only if they point to the same location in the same file.
// rust_span!() spans carry Rust file paths; user spans carry source file paths.
impl PartialEq for Span {
    fn eq(&self, other: &Self) -> bool {
        self.start == other.start
            && self.end == other.end
            && self.file.path == other.file.path
    }
}

impl Eq for Span {}

impl Hash for Span {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.start.hash(state);
        self.end.hash(state);
        self.file.path.hash(state);
    }
}

impl Span {
    /// Create a span with a mandatory source file.
    pub fn new(start: Position, end: Position, file: Arc<SourceFile>) -> Self {
        Self { start, end, file }
    }

    /// Create a span carrying Rust source location for synthetic nodes.
    ///
    /// Use the `rust_span!()` macro instead of calling this directly — it automatically
    /// passes `file!()` and `line!()` so error messages show the Rust source location
    /// where the synthetic node was created rather than the unhelpful `1:1-1:1`.
    pub fn rust_source(file: &'static str, line: u32) -> Self {
        let pos = Position {
            offset: 0,
            line: line as usize,
            column: 1,
        };
        Self {
            start: pos,
            end: pos,
            file: std::sync::Arc::new(SourceFile {
                path: std::sync::Arc::from(file),
                content: std::sync::Arc::from(""),
            }),
        }
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
    /// Literal pattern — int, float, bool, or string literal
    Literal(LiteralPattern),
    /// Pin pattern — bare lowercase name or `$name` in pattern position.
    ///
    /// T-1154: Previously, `$name` (escaped) was Pin and bare `name` was `Variable`.
    /// Now both produce `Pin`. Semantics:
    /// - If `name` is in scope: compare scrutinee against the scope value (equality check).
    /// - If `name` is not in scope: act as wildcard (always match, no binding introduced).
    ///
    /// To bind a name in a match arm, use `[case [let name] pattern body]`.
    ///
    /// `pin_resolution` carries the de Bruijn coordinates of `name` as resolved by the
    /// resolver. Semantics of the OnceLock value:
    /// - `None` (not set)           = resolver never ran on this node
    /// - `Some(None)`               = resolver ran; name was NOT in scope → wildcard
    /// - `Some(Some((level, slot)))` = resolver ran; name resolved to these coordinates
    ///
    /// Cloning resets the OnceLock (same as `Resolution::clone`), so cloned patterns
    /// must be re-resolved. This matches the contract for `Resolution` on VarRef nodes.
    Pin(String, Resolution),
    /// Dict pattern — matches dicts by key, binds matched values to pattern variables
    /// `rest: true` means open matching (extra keys allowed)
    /// `rest: false` means closed matching (extra keys rejected)
    Dict {
        fields: Vec<(String, Spanned<Pattern>)>,
        rest: bool,
    },
    /// Constructor pattern — matches nominal variants by tag, binds payload
    /// `[Maybe.Some v]` matches `Variant { tag: "Maybe.Some", payload }` and binds `v` to the payload
    /// `[Maybe.None]` (bracket form) matches `Variant { tag: "Maybe.None", payload: None }` via Constructor { binding: None }
    Constructor {
        tag: String,
        binding: Option<Box<Spanned<Pattern>>>,
    },
    /// TypeAssertPending pattern — surface form produced by the parser; rewritten to TypeAssert
    /// by the elaboration pass in typecheck_match.rs before type checking.
    ///
    /// `inner: None` = bare type assertion (currently unused by the parser; reserved for future forms)
    /// `inner: Some(pat)` = type-guarded binding (`[@Int x]` produces TypeAssertPending with inner=Variable("x"))
    TypeAssertPending {
        annotation: Spanned<Annotation>,
        inner: Option<Box<Spanned<Pattern>>>,
        /// Type resolved by the type checker. Set inline; read by the lowerer to convert to TypeAssert.
        resolved: TypeAnnotation,
    },
    /// TypeAssert pattern — core form used by the evaluator after elaboration.
    /// Created from TypeAssertPending by the elaboration pass (typecheck phase).
    TypeAssert {
        resolved_type: Type,
        inner: Option<Box<Spanned<Pattern>>>,
    },
    /// Or-pattern — matches if any sub-pattern matches
    /// Both branches must bind the same set of variables
    Or(Vec<Spanned<Pattern>>),
    /// Predicate pattern — a call expression whose head is a lowercase name or operator.
    ///
    /// At runtime, the full call expression is evaluated as a function, then called with
    /// the match scrutinee as its last positional argument. If the result is truthy (Int nonzero),
    /// the arm matches; otherwise the arm is skipped.
    ///
    /// `[contains? "ob"]` in pattern position → `Predicate { call: SurfaceNode for [contains? "ob"], .. }`.
    /// At match time: `[contains? "ob" scrutinee]` is evaluated.
    ///
    /// Predicate patterns do not introduce any variable bindings and do not count toward
    /// exhaustiveness analysis (treated as wildcard for coverage purposes).
    ///
    /// `to_match_binding` is resolved by the type checker to the Matchable instance's
    /// `to-match` method binding name (e.g., `"ɪɴꜱᴛᴀɴᴄᴇ⧼Matchable∷to-match⟨Boolean⟩⧽"`).
    /// The evaluator uses this to call the correct instance without dynamic dispatch.
    /// Empty if not yet resolved or type checking was skipped.
    Predicate {
        call: Arc<SurfaceNode>,
        to_match_binding: MatchableBinding,
    },
}

/// Literal pattern values
#[derive(Debug, Clone, PartialEq)]
pub enum LiteralPattern {
    Int(i64),
    /// Unsigned 64-bit integer (from `42u` literal patterns)
    U64(u64),
    Float(f64),
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
            SurfaceExpression::U64(n) => write!(f, "{n}u"),
            SurfaceExpression::Float(n) => {
                let s = n.to_string();
                if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                    write!(f, "{s}.0")
                } else {
                    write!(f, "{s}")
                }
            }
            SurfaceExpression::Str(s) => write!(f, "{s:?}"),
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
            SurfaceExpression::Placeholder => write!(f, "..."),
            SurfaceExpression::Rest(None, _) => write!(f, "..."),
            SurfaceExpression::Rest(Some(name), Some(ann)) => write!(f, "...{name}@{}", ann.node),
            SurfaceExpression::Rest(Some(name), None) => write!(f, "...{name}"),
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
            SurfaceExpression::Field { expr, field, .. } => match (expr, field) {
                (Some(e), DotKey::Ident(s)) => write!(f, "{}.{s}", e),
                (Some(e), DotKey::Int(n)) => write!(f, "{}.{n}", e),
                (None, DotKey::Ident(s)) => write!(f, ".{s}"),
                (None, DotKey::Int(n)) => write!(f, ".{n}"),
            },
            SurfaceExpression::Pipe { lhs, rhs } => write!(f, "{} | {}", lhs, rhs),
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

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pattern::Wildcard => write!(f, "_"),
            Pattern::Literal(lit) => write!(f, "{lit}"),

            Pattern::Pin(name, _) => write!(f, "{name}"),
            Pattern::TypeAssertPending {
                annotation, inner, ..
            } => {
                if let Some(inner) = inner {
                    write!(f, "[@{} {}]", annotation.node, inner.node)
                } else {
                    write!(f, "[@{}]", annotation.node)
                }
            }
            Pattern::TypeAssert { inner, .. } => {
                if let Some(inner) = inner {
                    write!(f, "[@<resolved> {}]", inner.node)
                } else {
                    write!(f, "[@<resolved>]")
                }
            }
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
            Pattern::Predicate { .. } => write!(f, "<predicate>"),
        }
    }
}

impl fmt::Display for LiteralPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LiteralPattern::Int(n) => write!(f, "{n}"),
            LiteralPattern::U64(n) => write!(f, "{n}u"),
            LiteralPattern::Float(n) => {
                let s = n.to_string();
                if !s.contains('.') && !s.contains('e') && !s.contains('E') {
                    write!(f, "{s}.0")
                } else {
                    write!(f, "{s}")
                }
            }
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


/// Returns true if `ann` is an `@Expr` annotation — `Simple("Expr")`.
///
/// Used by the evaluator (eval_materialize.rs) and type checker (typecheck_match.rs) to
/// identify params that receive raw quoted AST instead of evaluated values.
pub fn is_expr_annotation(ann: &Annotation) -> bool {
    matches!(ann, Annotation::Simple(s) if s == "Expr")
}

/// Default function for `Annotated.inner` when deserializing via `ExprConvert`.
/// Returns a placeholder `Arc<SurfaceNode>` wrapping a `Placeholder` expression.
/// This is only used when reconstructing an `Annotated` node from a dict repr that has no
/// `inner` field (e.g., deserialized from user-facing AST dict). Callers that need a real
/// inner VarRef must construct `Annotated` directly with the correct inner node.
pub fn annotated_inner_default() -> Arc<SurfaceNode> {
    Arc::new(SurfaceNode::new(
        SurfaceExpression::Placeholder,
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
            expr: SurfaceExpression::Placeholder,
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
    Str(#[expr(key = "value")] String),

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

    // Row variable / open record marker — None = unnamed (...), Some("name") = ...name.
    // The optional Annotation carries the type annotation from `...name@Type` syntax.
    #[expr(tag = "Rest")]
    Rest(
        #[expr(key = "name", string_opt)] Option<String>,
        #[expr(skip)] Option<Spanned<Annotation>>,
    ),

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

    // Placeholder `...` — evaluates to error when forced
    #[expr(tag = "Placeholder", unit)]
    Placeholder,

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

/// A match arm in a SurfaceExpression::Match.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceMatchArm {
    pub pattern: Spanned<Pattern>,
    pub guard: Option<Arc<SurfaceNode>>,
    pub body: Arc<SurfaceNode>,
    /// Compile-time-resolved Matchable instance binding name for the guard expression.
    /// Resolved by the type checker after the guard's return type is inferred.
    /// When resolved, the evaluator uses this for direct dispatch instead of call_to_match.
    pub guard_matchable_binding: MatchableBinding,
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
/// Stores the mangled instance binding name (e.g., `ɪɴꜱᴛᴀɴᴄᴇ⧼Addable∷+⟨Int,Int,Int⟩⧽`)
/// so the lowerer can emit a `CoreExpr::Var` with `level = u32::MAX, slot = u32::MAX`
/// and the mangled name, which the runtime resolves via name-based env chain lookup.
///
/// Written at most once by the type checker (after argument-type unification determines the
/// concrete instance). Read once by the lowerer when emitting the Call's function sub-expression.
/// Interior-mutable via `OnceLock` so the type checker can set it through a shared reference
/// to the `Arc<SurfaceNode>` that owns the VarRef.
pub struct CallDispatch(std::sync::OnceLock<String>);
impl CallDispatch {
    pub fn new() -> Self {
        Self(std::sync::OnceLock::new())
    }
    /// Returns the mangled instance binding name if set, or `None` if not yet dispatched.
    pub fn get(&self) -> Option<&str> {
        self.0.get().map(String::as_str)
    }
    /// Set the mangled instance binding name.  Silently ignores a second call (OnceLock
    /// semantics): the first write wins.  Call sites must ensure the write happens at most
    /// once per VarRef (guaranteed because type-checking is a single forward pass).
    pub fn set(&self, mangled_name: String) {
        let _ = self.0.set(mangled_name);
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

/// Inline de Bruijn coordinates for a VarRef or leading-dot Field node.
/// Written once by the resolver; read by the lowerer.
/// Clone resets to empty — cloned nodes are in new scopes and must be re-resolved.
pub struct Resolution(std::sync::OnceLock<Option<(u32, u32)>>);

impl Resolution {
    pub fn new() -> Self {
        Self(std::sync::OnceLock::new())
    }
    /// `Some(Some((level, slot)))` = resolved; `Some(None)` = unresolvable; `None` = not yet resolved.
    pub fn get(&self) -> Option<Option<(u32, u32)>> {
        self.0.get().copied()
    }
    /// Called by the resolver exactly once per node instance.
    pub fn set(&self, val: Option<(u32, u32)>) {
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
        write!(f, "Resolution({:?})", self.0.get())
    }
}

/// Table mapping each VarRef's NodeId to its resolved (level, slot) de Bruijn coordinates.
/// Produced by the resolver pass; consumed by the lowerer and type checker.
pub type ResolutionTable = std::collections::HashMap<NodeId, (u32, u32)>;

/// Table mapping each TypeAssert SurfaceNode's NodeId to its resolved Type.
/// Produced by the type checker; consumed by the lowerer to generate CoreExpr::TypeAssert.
pub type TypeAnnotationTable = std::collections::HashMap<NodeId, crate::types::Type>;

/// Inline annotation for a predicate pattern's resolved Matchable `to-match` instance
/// binding name. Written once by the type checker during match arm elaboration; read by
/// the lowerer to carry the binding name into `CoreMatchArm::Pattern::Predicate`.
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

    // VarRef with resolved de Bruijn coordinates
    Var {
        name: String,
        level: u32,
        slot: u32,
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
}

/// A match arm in a CoreExpr::Match.
#[derive(Debug, Clone)]
pub struct CoreMatchArm {
    pub pattern: Spanned<Pattern>,
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
            file: rust_span!().file,
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
}
