//! AST types: `File`, `Document`, `Expr`, `Entry`, `Param`, `Annotation`, `Spanned<T>`.

use crate::types::Type;
use std::cell::RefCell;
use std::fmt;

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
    /// display depth limits and initial `$$` values.
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
    pub expressions: Vec<Spanned<Expr>>,
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
    /// String literal (bare word or quoted), e.g. `hello` or `"hello"`
    Str(String),

    /// Variable reference, e.g. `$x` or `$$`
    VarRef(String),
    /// Dot access on an expression, e.g. `$a.b`
    DotAccess {
        expr: Box<Spanned<Expr>>,
        field: String,
    },
    /// Bracket key access, e.g. `$a[0]` or `$a[$key]`
    BracketAccess {
        expr: Box<Spanned<Expr>>,
        key: Box<Spanned<Expr>>,
    },
    /// Key-range slice, e.g. `$a[2..5]` or `$a[2..]`
    RangeAccess {
        expr: Box<Spanned<Expr>>,
        start: Option<Box<Spanned<Expr>>>,
        end: Option<Box<Spanned<Expr>>>,
    },

    /// Dict/list literal with keyed and/or auto-indexed entries
    Dict(Vec<Spanned<Entry>>),

    /// Function application via `[call $f ...]`
    Call {
        func: Box<Spanned<Expr>>,
        args: Vec<Spanned<Expr>>,
        named_args: Vec<Spanned<NamedArg>>,
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
        body: Box<Spanned<Expr>>,
        desugared: bool,
    },
    /// Type alias declaration via `[type expr]`
    TypeAlias(Box<Spanned<Expr>>),

    /// Type assertion, e.g. `[@Number $expr]`
    TypeAssert {
        annotation: Spanned<Annotation>,
        expr: Box<Spanned<Expr>>,
        /// Type resolved during elaboration (type checking).
        /// Uses RefCell for write-once elaboration without cloning the entire AST.
        /// The write-once invariant is enforced in typecheck.rs `resolve_type_assert`.
        resolved_type: RefCell<Option<Type>>,
    },

    /// Annotated bare word in value position, e.g. `Fn@Number`
    Annotated {
        name: String,
        annotation: Spanned<Annotation>,
    },

    /// Row variable / open record marker, e.g. `...` or `...rest`
    Rest(Option<String>),

    /// Parse error — a section of source that couldn't be parsed.
    /// Emitted by bracket-level error recovery (parser-rewrite.md §Phase 4).
    /// The span covers the entire unparseable region.
    Error(Span),
}

/// A dict entry (keyed or auto-indexed)
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: Option<Spanned<Expr>>,
    pub value: Spanned<Expr>,
}

/// A named argument in a call expression
#[derive(Debug, Clone, PartialEq)]
pub struct NamedArg {
    pub name: String,
    pub value: Spanned<Expr>,
}

/// A function parameter
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub annotation: Option<Spanned<Annotation>>,
    pub variadic: bool,
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
                    Expr::Str(name) if name == key => Some(&entry.node.value),
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

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Int(n) => write!(f, "{n}"),
            Expr::Float(n) => write!(f, "{n}"),
            Expr::Bool(b) => write!(f, "{b}"),
            Expr::Str(s) => write!(f, "{s:?}"),
            Expr::VarRef(name) => write!(f, "${name}"),
            Expr::DotAccess { expr, field } => write!(f, "{}.{field}", expr.node),
            Expr::BracketAccess { expr, key } => write!(f, "{}[{}]", expr.node, key.node),
            Expr::RangeAccess { expr, start, end } => {
                write!(f, "{}[", expr.node)?;
                if let Some(s) = start {
                    write!(f, "{}", s.node)?;
                }
                write!(f, "..")?;
                if let Some(e) = end {
                    write!(f, "{}", e.node)?;
                }
                write!(f, "]")
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
            } => {
                write!(f, "[call {}", func.node)?;
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
                write!(f, "] {}]", body.node)
            }
            Expr::TypeAlias(inner) => write!(f, "[type {}]", inner.node),
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

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}-{}:{}",
            self.start.line, self.start.column, self.end.line, self.end.column
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::sp;

    #[test]
    fn test_display_range_access_full() {
        let expr = Expr::RangeAccess {
            expr: Box::new(sp(Expr::VarRef("list".into()))),
            start: Some(Box::new(sp(Expr::Int(1)))),
            end: Some(Box::new(sp(Expr::Int(5)))),
        };
        assert_eq!(format!("{expr}"), "$list[1..5]");
    }

    #[test]
    fn test_display_range_access_start_only() {
        let expr = Expr::RangeAccess {
            expr: Box::new(sp(Expr::VarRef("list".into()))),
            start: Some(Box::new(sp(Expr::Int(2)))),
            end: None,
        };
        assert_eq!(format!("{expr}"), "$list[2..]");
    }

    #[test]
    fn test_display_range_access_end_only() {
        let expr = Expr::RangeAccess {
            expr: Box::new(sp(Expr::VarRef("list".into()))),
            start: None,
            end: Some(Box::new(sp(Expr::Int(10)))),
        };
        assert_eq!(format!("{expr}"), "$list[..10]");
    }

    #[test]
    fn test_display_range_access_unbounded() {
        let expr = Expr::RangeAccess {
            expr: Box::new(sp(Expr::VarRef("list".into()))),
            start: None,
            end: None,
        };
        assert_eq!(format!("{expr}"), "$list[..]");
    }

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
                value: sp(Expr::Str("Number".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("min".into()))),
                value: sp(Expr::Int(0)),
            }),
        ];
        let expr = Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::VarRef("x".into()))),
            resolved_type: RefCell::new(None),
        };
        assert_eq!(
            format!("{expr}"),
            "[@[\"type\": \"Number\"  \"min\": 0] $x]"
        );
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
            key: Some(sp(Expr::VarRef("required".into()))),
            value: sp(Expr::Bool(true)),
        })];
        let expr = Expr::Annotated {
            name: "port".into(),
            annotation: sp(Annotation::PropertyDict(entries)),
        };
        assert_eq!(format!("{expr}"), "port@[$required: true]");
    }

    #[test]
    fn test_display_call_no_args() {
        let expr = Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![],
            named_args: vec![],
        };
        assert_eq!(format!("{expr}"), "[call $f]");
    }

    #[test]
    fn test_display_call_with_args() {
        let expr = Expr::Call {
            func: Box::new(sp(Expr::VarRef("add".into()))),
            args: vec![sp(Expr::Int(1)), sp(Expr::Int(2))],
            named_args: vec![],
        };
        assert_eq!(format!("{expr}"), "[call $add 1 2]");
    }

    #[test]
    fn test_display_call_with_named_args() {
        let expr = Expr::Call {
            func: Box::new(sp(Expr::VarRef("config".into()))),
            args: vec![],
            named_args: vec![sp(NamedArg {
                name: "port".into(),
                value: sp(Expr::Int(8080)),
            })],
        };
        assert_eq!(format!("{expr}"), "[call $config port: 8080]");
    }

    #[test]
    fn test_display_call_with_both_arg_types() {
        let expr = Expr::Call {
            func: Box::new(sp(Expr::VarRef("deploy".into()))),
            args: vec![sp(Expr::Str("prod".into()))],
            named_args: vec![sp(NamedArg {
                name: "replicas".into(),
                value: sp(Expr::Int(3)),
            })],
        };
        assert_eq!(format!("{expr}"), "[call $deploy \"prod\" replicas: 3]");
    }

    #[test]
    fn test_display_fn_no_params_no_return() {
        let expr = Expr::Fn {
            return_ann: None,
            params: vec![],
            body: Box::new(sp(Expr::Int(42))),
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
            body: Box::new(sp(Expr::VarRef("x".into()))),
            desugared: false,
        };
        assert_eq!(format!("{expr}"), "[fn [x y] $x]");
    }

    #[test]
    fn test_display_fn_with_return_annotation() {
        let expr = Expr::Fn {
            return_ann: Some(sp(Annotation::Simple("Number".into()))),
            params: vec![],
            body: Box::new(sp(Expr::Int(0))),
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
            body: Box::new(sp(Expr::VarRef("x".into()))),
            desugared: false,
        };
        assert_eq!(format!("{expr}"), "[fn [x@Int] $x]");
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
            body: Box::new(sp(Expr::VarRef("args".into()))),
            desugared: false,
        };
        assert_eq!(format!("{expr}"), "[fn [...args] $args]");
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
                key: Some(sp(Expr::VarRef("a".into()))),
                value: sp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::VarRef("b".into()))),
                value: sp(Expr::Int(2)),
            }),
        ]);
        assert_eq!(format!("{expr}"), "[$a: 1  $b: 2]");
    }

    #[test]
    fn test_display_dict_with_auto_indexed_entries() {
        let expr = Expr::Dict(vec![
            sp(Entry {
                key: None,
                value: sp(Expr::Int(10)),
            }),
            sp(Entry {
                key: None,
                value: sp(Expr::Int(20)),
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
                value: sp(Expr::Str("Number".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: sp(Expr::Int(42)),
            }),
        ]);
        assert_eq!(format!("{ann}"), "[\"type\": \"Number\"  \"default\": 42]");
    }

    #[test]
    fn test_display_document_single_expression() {
        let doc = Document {
            expressions: vec![sp(Expr::Int(42))],
        };
        assert_eq!(format!("{doc}"), "42");
    }

    #[test]
    fn test_display_document_multiple_expressions() {
        let doc = Document {
            expressions: vec![
                sp(Expr::VarRef("x".into())),
                sp(Expr::Int(10)),
                sp(Expr::Bool(true)),
            ],
        };
        assert_eq!(format!("{doc}"), "$x\n10\ntrue");
    }

    #[test]
    fn test_display_document_empty() {
        let doc = Document {
            expressions: vec![],
        };
        assert_eq!(format!("{doc}"), "");
    }

    #[test]
    fn test_display_file_single_document() {
        let file = File {
            documents: vec![sp(Document {
                expressions: vec![sp(Expr::Int(1))],
            })],
        };
        assert_eq!(format!("{file}"), "1");
    }

    #[test]
    fn test_display_file_multiple_documents() {
        let file = File {
            documents: vec![
                sp(Document {
                    expressions: vec![sp(Expr::Int(1))],
                }),
                sp(Document {
                    expressions: vec![sp(Expr::Int(2))],
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
