use std::fmt;

/// Byte offset + line/column position in source text
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub offset: usize,
    pub line: usize,
    pub column: usize,
}

/// Source span (start..end)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub start: Position,
    pub end: Position,
}

impl Span {
    pub fn new(start: Position, end: Position) -> Self {
        Self { start, end }
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

/// A complete LLT file — one or more documents separated by ---
#[derive(Debug, Clone, PartialEq)]
pub struct File {
    pub documents: Vec<Spanned<Document>>,
}

/// A document — one or more expressions forming a scope chain
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    pub expressions: Vec<Spanned<Expr>>,
}

/// The central expression type
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    // Literals
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),

    // References and access
    VarRef(String),
    DotAccess {
        expr: Box<Spanned<Expr>>,
        field: String,
    },
    BracketAccess {
        expr: Box<Spanned<Expr>>,
        key: Box<Spanned<Expr>>,
    },
    RangeAccess {
        expr: Box<Spanned<Expr>>,
        start: Option<Box<Spanned<Expr>>>,
        end: Option<Box<Spanned<Expr>>>,
    },

    // Data
    Dict(Vec<Spanned<Entry>>),

    // Special forms
    Call {
        func: Box<Spanned<Expr>>,
        args: Vec<Spanned<Expr>>,
        named_args: Vec<Spanned<NamedArg>>,
    },
    Fn {
        return_ann: Option<Spanned<Annotation>>,
        params: Vec<Spanned<Param>>,
        body: Box<Spanned<Expr>>,
    },
    TypeAlias(Box<Spanned<Expr>>),

    // Type expressions
    TypeAssert {
        annotation: Spanned<Annotation>,
        expr: Box<Spanned<Expr>>,
    },

    // Generalized annotation in value position
    Annotated {
        name: String,
        annotation: Spanned<Annotation>,
    },
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

// --- Display implementations for display and error reporting ---

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
            Expr::TypeAssert { annotation, expr } => {
                write!(f, "[@{} {}]", annotation.node, expr.node)
            }
            Expr::Annotated { name, annotation } => {
                write!(f, "{name}@{}", annotation.node)
            }
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

    fn test_spanned<T>(node: T) -> Spanned<T> {
        sp(node)
    }

    // -- Expr::Display tests --

    #[test]
    fn test_display_range_access_full() {
        let expr = Expr::RangeAccess {
            expr: Box::new(test_spanned(Expr::VarRef("list".into()))),
            start: Some(Box::new(test_spanned(Expr::Int(1)))),
            end: Some(Box::new(test_spanned(Expr::Int(5)))),
        };
        assert_eq!(format!("{expr}"), "$list[1..5]");
    }

    #[test]
    fn test_display_range_access_start_only() {
        let expr = Expr::RangeAccess {
            expr: Box::new(test_spanned(Expr::VarRef("list".into()))),
            start: Some(Box::new(test_spanned(Expr::Int(2)))),
            end: None,
        };
        assert_eq!(format!("{expr}"), "$list[2..]");
    }

    #[test]
    fn test_display_range_access_end_only() {
        let expr = Expr::RangeAccess {
            expr: Box::new(test_spanned(Expr::VarRef("list".into()))),
            start: None,
            end: Some(Box::new(test_spanned(Expr::Int(10)))),
        };
        assert_eq!(format!("{expr}"), "$list[..10]");
    }

    #[test]
    fn test_display_range_access_unbounded() {
        let expr = Expr::RangeAccess {
            expr: Box::new(test_spanned(Expr::VarRef("list".into()))),
            start: None,
            end: None,
        };
        assert_eq!(format!("{expr}"), "$list[..]");
    }

    #[test]
    fn test_display_type_assert() {
        let expr = Expr::TypeAssert {
            annotation: test_spanned(Annotation::Simple("Number".into())),
            expr: Box::new(test_spanned(Expr::Int(42))),
        };
        assert_eq!(format!("{expr}"), "[@Number 42]");
    }

    #[test]
    fn test_display_type_assert_with_property_dict() {
        let entries = vec![
            test_spanned(Entry {
                key: Some(test_spanned(Expr::VarRef("type".into()))),
                value: test_spanned(Expr::VarRef("Number".into())),
            }),
            test_spanned(Entry {
                key: Some(test_spanned(Expr::VarRef("min".into()))),
                value: test_spanned(Expr::Int(0)),
            }),
        ];
        let expr = Expr::TypeAssert {
            annotation: test_spanned(Annotation::PropertyDict(entries)),
            expr: Box::new(test_spanned(Expr::VarRef("x".into()))),
        };
        assert_eq!(format!("{expr}"), "[@[$type: $Number  $min: 0] $x]");
    }

    #[test]
    fn test_display_annotated() {
        let expr = Expr::Annotated {
            name: "Config".into(),
            annotation: test_spanned(Annotation::Simple("ConfigType".into())),
        };
        assert_eq!(format!("{expr}"), "Config@ConfigType");
    }

    #[test]
    fn test_display_annotated_with_property_dict() {
        let entries = vec![test_spanned(Entry {
            key: Some(test_spanned(Expr::VarRef("required".into()))),
            value: test_spanned(Expr::Bool(true)),
        })];
        let expr = Expr::Annotated {
            name: "port".into(),
            annotation: test_spanned(Annotation::PropertyDict(entries)),
        };
        assert_eq!(format!("{expr}"), "port@[$required: true]");
    }

    #[test]
    fn test_display_call_no_args() {
        let expr = Expr::Call {
            func: Box::new(test_spanned(Expr::VarRef("f".into()))),
            args: vec![],
            named_args: vec![],
        };
        assert_eq!(format!("{expr}"), "[call $f]");
    }

    #[test]
    fn test_display_call_with_args() {
        let expr = Expr::Call {
            func: Box::new(test_spanned(Expr::VarRef("add".into()))),
            args: vec![test_spanned(Expr::Int(1)), test_spanned(Expr::Int(2))],
            named_args: vec![],
        };
        assert_eq!(format!("{expr}"), "[call $add 1 2]");
    }

    #[test]
    fn test_display_call_with_named_args() {
        let expr = Expr::Call {
            func: Box::new(test_spanned(Expr::VarRef("config".into()))),
            args: vec![],
            named_args: vec![test_spanned(NamedArg {
                name: "port".into(),
                value: test_spanned(Expr::Int(8080)),
            })],
        };
        assert_eq!(format!("{expr}"), "[call $config port: 8080]");
    }

    #[test]
    fn test_display_call_with_both_arg_types() {
        let expr = Expr::Call {
            func: Box::new(test_spanned(Expr::VarRef("deploy".into()))),
            args: vec![test_spanned(Expr::Str("prod".into()))],
            named_args: vec![test_spanned(NamedArg {
                name: "replicas".into(),
                value: test_spanned(Expr::Int(3)),
            })],
        };
        assert_eq!(format!("{expr}"), "[call $deploy \"prod\" replicas: 3]");
    }

    #[test]
    fn test_display_fn_no_params_no_return() {
        let expr = Expr::Fn {
            return_ann: None,
            params: vec![],
            body: Box::new(test_spanned(Expr::Int(42))),
        };
        assert_eq!(format!("{expr}"), "[fn [] 42]");
    }

    #[test]
    fn test_display_fn_with_params() {
        let expr = Expr::Fn {
            return_ann: None,
            params: vec![
                test_spanned(Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                }),
                test_spanned(Param {
                    name: "y".into(),
                    annotation: None,
                    variadic: false,
                }),
            ],
            body: Box::new(test_spanned(Expr::VarRef("x".into()))),
        };
        assert_eq!(format!("{expr}"), "[fn [x y] $x]");
    }

    #[test]
    fn test_display_fn_with_return_annotation() {
        let expr = Expr::Fn {
            return_ann: Some(test_spanned(Annotation::Simple("Number".into()))),
            params: vec![],
            body: Box::new(test_spanned(Expr::Int(0))),
        };
        assert_eq!(format!("{expr}"), "[fn@Number [] 0]");
    }

    #[test]
    fn test_display_fn_with_annotated_params() {
        let expr = Expr::Fn {
            return_ann: None,
            params: vec![test_spanned(Param {
                name: "x".into(),
                annotation: Some(test_spanned(Annotation::Simple("Int".into()))),
                variadic: false,
            })],
            body: Box::new(test_spanned(Expr::VarRef("x".into()))),
        };
        assert_eq!(format!("{expr}"), "[fn [x@Int] $x]");
    }

    #[test]
    fn test_display_fn_with_variadic_param() {
        let expr = Expr::Fn {
            return_ann: None,
            params: vec![test_spanned(Param {
                name: "args".into(),
                annotation: None,
                variadic: true,
            })],
            body: Box::new(test_spanned(Expr::VarRef("args".into()))),
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
            test_spanned(Entry {
                key: Some(test_spanned(Expr::VarRef("a".into()))),
                value: test_spanned(Expr::Int(1)),
            }),
            test_spanned(Entry {
                key: Some(test_spanned(Expr::VarRef("b".into()))),
                value: test_spanned(Expr::Int(2)),
            }),
        ]);
        assert_eq!(format!("{expr}"), "[$a: 1  $b: 2]");
    }

    #[test]
    fn test_display_dict_with_auto_indexed_entries() {
        let expr = Expr::Dict(vec![
            test_spanned(Entry {
                key: None,
                value: test_spanned(Expr::Int(10)),
            }),
            test_spanned(Entry {
                key: None,
                value: test_spanned(Expr::Int(20)),
            }),
        ]);
        assert_eq!(format!("{expr}"), "[10  20]");
    }

    // -- Annotation::PropertyDict::Display tests --

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
        let ann = Annotation::PropertyDict(vec![
            test_spanned(Entry {
                key: Some(test_spanned(Expr::VarRef("type".into()))),
                value: test_spanned(Expr::VarRef("Number".into())),
            }),
            test_spanned(Entry {
                key: Some(test_spanned(Expr::VarRef("default".into()))),
                value: test_spanned(Expr::Int(42)),
            }),
        ]);
        assert_eq!(format!("{ann}"), "[$type: $Number  $default: 42]");
    }

    // -- Document::Display tests --

    #[test]
    fn test_display_document_single_expression() {
        let doc = Document {
            expressions: vec![test_spanned(Expr::Int(42))],
        };
        assert_eq!(format!("{doc}"), "42");
    }

    #[test]
    fn test_display_document_multiple_expressions() {
        let doc = Document {
            expressions: vec![
                test_spanned(Expr::VarRef("x".into())),
                test_spanned(Expr::Int(10)),
                test_spanned(Expr::Bool(true)),
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
            documents: vec![test_spanned(Document {
                expressions: vec![test_spanned(Expr::Int(1))],
            })],
        };
        assert_eq!(format!("{file}"), "1");
    }

    #[test]
    fn test_display_file_multiple_documents() {
        let file = File {
            documents: vec![
                test_spanned(Document {
                    expressions: vec![test_spanned(Expr::Int(1))],
                }),
                test_spanned(Document {
                    expressions: vec![test_spanned(Expr::Int(2))],
                }),
            ],
        };
        assert_eq!(format!("{file}"), "1\n---\n2");
    }
}
