//! TypeValue constructor identity constants and display-name lookup.
//!
//! Constructor identity is numeric: `(type_decl_id, ctor_index)`. The `type_decl_id`
//! identifies the type declaration; `ctor_index` is the zero-based position of the
//! constructor within that declaration. Display names are derived via lookup only —
//! strings are opaque human-readable representations, never used for identity.
//!
//! The canonical source of truth for constructor ordering is `stdlib/builtin_core.llt`
//! (the `TypeValue: [type ...]` declaration). Constants in the "declared in
//! builtin_core.llt" sections must match the declaration order exactly.
//! Rust-internal-only sentinels are assigned indices after the declared constructors.

// ── TypeValue ctor_index (builtin_core.llt declaration order) ────────────────

pub const TV_REPR: u32 = 0;
pub const TV_PHANTOM: u32 = 1;
pub const TV_VAR: u32 = 2;
pub const TV_FN: u32 = 3;
pub const TV_APP: u32 = 4;
pub const TV_OP: u32 = 5;
pub const TV_RECORD: u32 = 6;
pub const TV_UNION: u32 = 7;
pub const TV_INTER: u32 = 8;
pub const TV_NEG: u32 = 9;
pub const TV_SCHEME: u32 = 10;
pub const TV_INT_LIT: u32 = 11;
pub const TV_FLOAT_LIT: u32 = 12;
pub const TV_STR_LIT: u32 = 13;
pub const TV_RECURSIVE: u32 = 14;
pub const TV_RECURSIVE_REF: u32 = 15;
pub const TV_NOMINAL_VARIANT: u32 = 16;
pub const TV_UNKNOWN: u32 = 17;
pub const TV_NEVER: u32 = 18;
pub const TV_TOP: u32 = 19;

// ── Rust-internal-only TypeValue sentinels (not declared in builtin_core.llt) ──

pub const TV_STAGE_APP: u32 = 20;
pub const TV_ERROR: u32 = 21;
pub const TV_NOMINAL: u32 = 22;

// ── RowTail ctor_index ──────────────────────────────────────────────────────

pub const RT_CLOSED: u32 = 0;
pub const RT_VAR: u32 = 1;
pub const RT_UNIFORM: u32 = 2;

// ── RowTail payload field constants ─────────────────────────────────────────
//
// These are distinct from the TypeNode payload field constants (TN_FIELD_*)
// even when the string values coincide. If builtin_core.llt ever renames the
// RowTail.Uniform payload fields independently of the TypeNode.Dict fields,
// only these constants need updating.

/// Field key for RowTail.Uniform payload: `{ value-type: TypeValue }`
pub const RT_FIELD_VALUE_TYPE: &str = "value-type";
/// Field key for RowTail payload (reserved for typed-key maps): `{ key-type: TypeValue }`
pub const RT_FIELD_KEY_TYPE: &str = "key-type";

// ── ConstraintDecl (single-ctor structural type) ────────────────────────────

pub const CD_CONSTRAINT_DECL: u32 = 0;

// ── VarDecl (single-ctor structural type) ───────────────────────────────────

pub const VD_VAR_DECL: u32 = 0;

// ── Bool ctor_index ─────────────────────────────────────────────────────────

pub const BOOL_TRUE_IDX: u32 = 0;
pub const BOOL_FALSE_IDX: u32 = 1;

// ── ProgramItem ctor_index ──────────────────────────────────────────────────

pub const PI_FILE: u32 = 0;
pub const PI_EXPR: u32 = 1;

// ── TypeNode ctor_index (builtin_core.llt order) ────────────────────────────

pub const TN_INT: u32 = 0;
pub const TN_STRING: u32 = 1;
pub const TN_FLOAT: u32 = 2;
pub const TN_BYTES: u32 = 3;
pub const TN_NEVER: u32 = 4;
pub const TN_TOP: u32 = 5;
pub const TN_UNKNOWN: u32 = 6;
pub const TN_PROXY: u32 = 7;
pub const TN_CALLABLE: u32 = 8;
pub const TN_ABSENT: u32 = 9;
pub const TN_UNION: u32 = 10;
pub const TN_INTERSECT: u32 = 11;
pub const TN_NEGATION: u32 = 12;
pub const TN_ARROW: u32 = 13;
pub const TN_RECURSIVE: u32 = 14;
pub const TN_RECURSIVE_REF: u32 = 15;
pub const TN_DICT: u32 = 16;
pub const TN_TYPE_CONSTRUCTOR: u32 = 17;
pub const TN_TYPE_APPLICATION: u32 = 18;
pub const TN_TYPE_VAR: u32 = 19;
pub const TN_INT_LITERAL: u32 = 20;
pub const TN_STRING_LITERAL: u32 = 21;
pub const TN_FLOAT_LITERAL: u32 = 22;

// ── Expr ctor_index (ast.llt order + Rust-internal) ─────────────────────────

pub const EXPR_LITERAL: u32 = 0;
pub const EXPR_VARREF: u32 = 1;
pub const EXPR_DOT_ACCESS: u32 = 2;
pub const EXPR_PIPE: u32 = 3;
pub const EXPR_SEQUENTIAL: u32 = 4;
pub const EXPR_DICT: u32 = 5;
pub const EXPR_CALL: u32 = 6;
pub const EXPR_FN: u32 = 7;
pub const EXPR_TYPE_ALIAS: u32 = 8;
pub const EXPR_TYPE_ASSERT: u32 = 9;
pub const EXPR_ANNOTATED: u32 = 10;
pub const EXPR_REST: u32 = 11;
pub const EXPR_MATCH: u32 = 12;
pub const EXPR_LET_DECL: u32 = 13;
pub const EXPR_CASE_ARM: u32 = 14;
pub const EXPR_QUOTE: u32 = 15;
pub const EXPR_UNQUOTE: u32 = 16;
pub const EXPR_UNQUOTE_SPLICE: u32 = 17;
pub const EXPR_SPLICE: u32 = 18;
pub const EXPR_SYNTAX_CLASS: u32 = 19;
pub const EXPR_CLASS_DECL: u32 = 20;
pub const EXPR_INSTANCE_DECL: u32 = 21;
pub const EXPR_PATTERN_DECL: u32 = 22;
pub const EXPR_PLACEHOLDER: u32 = 23;
pub const EXPR_ERROR: u32 = 24;
// Rust-internal Expr variants (not in ast.llt)
pub const EXPR_ENTRY: u32 = 25;
pub const EXPR_NAMED_ARG: u32 = 26;
pub const EXPR_PARAMETER: u32 = 27;

// ── Entry ctor_index ────────────────────────────────────────────────────────

pub const ENTRY_KEYED: u32 = 0;
pub const ENTRY_UNKEYED: u32 = 1;

// ── Annotation ctor_index ───────────────────────────────────────────────────

pub const ANN_SIMPLE: u32 = 0;
pub const ANN_PROP_DICT: u32 = 1;
pub const ANN_NULL: u32 = 2;
// Rust-internal
pub const ANN_ANNOTATED: u32 = 3;
pub const ANN_UNKNOWN: u32 = 4;

// ── DotKey ctor_index ───────────────────────────────────────────────────────

pub const DOTKEY_IDENT: u32 = 0;
pub const DOTKEY_INDEX: u32 = 1;

// ── TypeValue payload field name constants ───────────────────────────────────
//
// Canonical field names used in TypeValue payload dicts. Each must match the
// field name declared in `stdlib/builtin_core.llt`.

/// Field key for TypeValue.Var: { name: String }
pub const FIELD_NAME: &str = "name";
/// Field key for TypeValue.Nominal: { id: u64 }
pub const FIELD_ID: &str = "id";
/// Field key for TypeValue.IntLit / TypeValue.StrLit / TypeValue.FloatLit: { value: T }
pub const FIELD_VALUE: &str = "value";
/// Field key for TypeValue.Fn params: { params: Dict }
pub const FIELD_PARAMS: &str = "params";
/// Field key for TypeValue.Fn return: { return: TypeValue }
pub const FIELD_RETURN: &str = "return";
/// Field key for TypeValue.Neg: { of: TypeValue }
pub const FIELD_OF: &str = "of";
/// Field key for TypeValue.App: { op: TypeValue }
pub const FIELD_OP: &str = "op";
/// Field key for TypeValue.App: { arg: TypeValue }
pub const FIELD_ARG: &str = "arg";
/// Field key for TypeValue.Record: { fields: Dict }
pub const FIELD_FIELDS: &str = "fields";
/// Field key for TypeValue.Record: { tail: TypeValue | [] }
pub const FIELD_TAIL: &str = "tail";
/// Field key for TypeValue.Union / TypeValue.Inter: { members: Dict }
pub const FIELD_MEMBERS: &str = "members";
/// Field key for TypeValue.NominalVariant: { tycon: String }
pub const FIELD_TYCON: &str = "tycon";
/// Field key for TypeValue.NominalVariant: { ctor: String }
pub const FIELD_CTOR: &str = "ctor";
/// Field key for TypeValue.Recursive: { body: TypeValue }
pub const FIELD_BODY: &str = "body";
/// Field key for ConstraintDecl: { class: TypeValue }
pub const FIELD_CLASS: &str = "class";
/// Field key for ConstraintDecl: { args: Dict }
pub const FIELD_ARGS: &str = "args";
/// Field key for TypeValue.Fn: { variadic: Bool }
pub const FIELD_VARIADIC: &str = "variadic";
/// Field key for TypeValue.Fn: { typed-variadics: Dict }
///
/// Stores the typed variadic buckets for functions declared with `...xs@Seq[T]` params.
/// The payload is an integer-keyed dict where each entry is a two-element dict:
/// `{ name: String, ty: TypeValue }` (the bucket name and its Seq[T] element type).
pub const FIELD_TYPED_VARIADICS: &str = "typed-variadics";
/// Field key for TypeValue.Fn: { param-names: Dict }
pub const FIELD_PARAM_NAMES: &str = "param-names";
/// Field key for TypeValue.Fn: { required: Integer }
///
/// The number of required (non-default) fixed parameters. Used for arity checking
/// to distinguish functions with optional trailing params from those with all required params.
pub const FIELD_REQUIRED: &str = "required";
/// Field key for TypeValue.RecursiveRef: { depth: Integer }
pub const FIELD_DEPTH: &str = "depth";
/// Field key for TypeValue.Scheme: { vars: Dict }
pub const FIELD_VARS: &str = "vars";
/// Field key for TypeValue.Scheme: { constraints: Dict }
pub const FIELD_CONSTRAINTS: &str = "constraints";
/// Field key for TypeValue.Scheme: { narrowings: Dict }
pub const FIELD_NARROWINGS: &str = "narrowings";
/// Field key for TypeValue.Scheme: { doc: String }
pub const FIELD_DOC: &str = "doc";
/// Field key for VarDecl payload: { kind: TypeValue }
pub const FIELD_KIND: &str = "kind";
/// Field key for TypeValue.Repr payload: { is: Fn | [] } — optional typeclass instance predicate
pub const FIELD_IS: &str = "is";

// ── Internal gensym prefix characters ───────────────────────────────────────
//
// The type checker prefixes internally-generated binding names with special
// Unicode characters to distinguish them from user-defined names. These
// constants are the first characters of each prefix family. Checking
// name.starts_with(INTERNAL_PREFIX_*) identifies whether a binding is
// synthetic and should be excluded from user-facing diagnostics (e.g.
// lost-binding warnings).
//
// If a gensym prefix ever changes, updating the constant here keeps all
// filter sites consistent.

/// First character of instance-binding gensym names: `ɪ` (U+026A LATIN LETTER SMALL CAPITAL I).
/// Instance bindings are named `ɪɴꜱᴛᴀɴᴄᴇ⧼{class}∷{method}⟨{args}⟩⧽`.
pub const INTERNAL_PREFIX_INSTANCE: char = 'ɪ';

/// First character of label-binding gensym names: `ʟ` (U+029F LATIN LETTER SMALL CAPITAL L).
pub const INTERNAL_PREFIX_LABEL: char = 'ʟ';

// ── TypeNode payload field name constants ────────────────────────────────────
//
// Field names in TypeNode payload dicts. Each must match the field name
// declared in `stdlib/builtin_core.llt` exactly. These are distinct from the
// TypeValue payload field constants above — TypeNode fields are only used
// during TypeNode-to-TypeValue conversion (type_normalize.rs, typecheck_annot.rs).

/// TypeNode.Union / TypeNode.Intersect payload field: `{ types: [Seq TypeNode] }`
pub const TN_FIELD_TYPES: &str = "types";
/// TypeNode.Negation payload field: `{ inner: TypeNode }`
pub const TN_FIELD_INNER: &str = "inner";
/// TypeNode.Dict payload field: `{ open: Bool }`
pub const TN_FIELD_OPEN: &str = "open";
/// TypeNode.Dict payload field: `{ fields: [Map String TypeNode] }`
///
/// Note: coincidentally the same string as `FIELD_FIELDS` (TypeValue.Record's `fields:` key),
/// but semantically distinct — this names the TypeNode.Dict payload field, not the TypeValue.Record
/// payload field. Keep separate constants to make call sites self-documenting.
pub const TN_FIELD_FIELDS: &str = "fields";
/// TypeNode.Arrow payload field: `{ params: [Seq TypeNode] }`
pub const TN_FIELD_PARAMS: &str = "params";
/// TypeNode.Arrow payload field: `{ result: TypeNode }`
pub const TN_FIELD_RESULT: &str = "result";
/// TypeNode.Dict payload field: `{ key-type: TypeNode }` (optional; enables typed-key maps)
pub const TN_FIELD_KEY_TYPE: &str = "key-type";
/// TypeNode.Dict payload field: `{ value-type: TypeNode }` (optional; typed-value maps)
pub const TN_FIELD_VALUE_TYPE: &str = "value-type";
/// TypeNode.TypeApplication payload field: `{ ctor: TypeNode }` (the type constructor)
pub const TN_FIELD_CTOR: &str = "ctor";
/// TypeNode.TypeApplication payload field: `{ args: Seq TypeNode }` (the type arguments)
///
/// Note: coincidentally the same string as `FIELD_ARGS` (ConstraintDecl's `args:` key),
/// but semantically distinct. Keep separate constants to make call sites self-documenting.
pub const TN_FIELD_ARGS: &str = "args";
/// TypeNode.TypeVar / TypeNode.TypeConstructor payload field: `{ name: String }`
///
/// Distinct from `FIELD_NAME` which is TypeValue.Op/Var's name field — kept separate
/// to make TypeNode vs TypeValue call sites self-documenting.
pub const TN_FIELD_NAME: &str = "name";
/// TypeNode.Recursive payload field: `{ var: String }` (the μ-binder name)
pub const TN_FIELD_VAR: &str = "var";
/// TypeNode.Recursive payload field: `{ body: TypeNode }` (the recursive body)
///
/// Note: coincidentally the same string as `FIELD_BODY` (TypeValue.Recursive's `body:` key)
/// but on a TypeNode payload rather than a TypeValue payload. Keep separate constants.
pub const TN_FIELD_BODY: &str = "body";
/// TypeNode.TypeVar payload field: `{ kind: String }` (e.g. "Type", "Operator", "Label")
pub const TN_FIELD_KIND: &str = "kind";
/// TypeNode.IntLiteral payload field: `{ n: Int }`
pub const TN_FIELD_N: &str = "n";
/// TypeNode.FloatLit payload field: `{ value: Float }`
///
/// Note: coincidentally same string as `FIELD_VALUE` (TypeValue literal payload field)
/// but on a TypeNode payload. Keep separate.
pub const TN_FIELD_FLOAT_VALUE: &str = "value";
/// TypeNode.StringLiteral payload field: `{ s: String }`
pub const TN_FIELD_S: &str = "s";

// ── Bare TypeNode constructor names ──────────────────────────────────────────
//
// After stripping the "TypeNode." prefix, the bare constructor name is used as
// the key in `typenode_value_to_type`'s inner dispatch (typecheck_annot.rs).
// These constants must match the constructor names in `stdlib/builtin_core.llt`
// exactly. If a TypeNode constructor is renamed there, update this section and
// the corresponding TN_* fully-qualified constant above.

pub const TN_BARE_UNKNOWN: &str = "Unknown";
pub const TN_BARE_TOP: &str = "Top";
pub const TN_BARE_NEVER: &str = "Never";
pub const TN_BARE_ABSENT: &str = "Absent";
pub const TN_BARE_INT: &str = "Int";
pub const TN_BARE_FLOAT: &str = "Float";
pub const TN_BARE_STRING: &str = "String";
pub const TN_BARE_BYTES: &str = "Bytes";
pub const TN_BARE_PROXY: &str = "Proxy";
pub const TN_BARE_CALLABLE: &str = "Callable";
pub const TN_BARE_UNION: &str = "Union";
pub const TN_BARE_INTERSECT: &str = "Intersect";
pub const TN_BARE_NEGATION: &str = "Negation";
pub const TN_BARE_DICT: &str = "Dict";
pub const TN_BARE_ARROW: &str = "Arrow";
pub const TN_BARE_TYPE_CONSTRUCTOR: &str = "TypeConstructor";
pub const TN_BARE_TYPE_APPLICATION: &str = "TypeApplication";
pub const TN_BARE_TYPE_VAR: &str = "TypeVar";
pub const TN_BARE_RECURSIVE: &str = "Recursive";
pub const TN_BARE_RECURSIVE_REF: &str = "RecursiveRef";
pub const TN_BARE_INT_LITERAL: &str = "IntLiteral";
pub const TN_BARE_FLOAT_LIT: &str = "FloatLiteral";
pub const TN_BARE_STRING_LITERAL: &str = "StringLiteral";

// ── Kind name constants ───────────────────────────────────────────────────────
//
// Values for the `kind:` field of TypeNode.TypeVar payloads, declared in
// `stdlib/builtin_core.llt`. These are also the strings accepted by
// `typenode_typevar_kind()` in type_normalize.rs.

pub const KIND_TYPE: &str = "Type";
pub const KIND_OPERATOR: &str = "Operator";
pub const KIND_LABEL: &str = "Label";
pub const KIND_ARROW: &str = "Arrow";

// ── TypeValue.Op name constants ───────────────────────────────────────────────
//
// The `name:` payload of `TypeValue.Op` variants for opaque builtin types.
// These are returned by `typevalue_op_name()` and used in `type_to_dispatch_tag()`
// for typeclass instance dispatch. If an opaque type is renamed in builtin_core.llt,
// update the corresponding TN_* constant above AND the OP_* constant here.

pub const OP_INT: &str = "Int";
pub const OP_U64: &str = "U64";
pub const OP_STR: &str = "Str";
pub const OP_FLOAT: &str = "Float";
pub const OP_BYTES: &str = "Bytes";
pub const OP_BOOL: &str = "Bool";
pub const OP_DICT: &str = "Dict";
pub const OP_FN: &str = "Fn";
pub const OP_FUNCTION: &str = "Function";
pub const OP_PROGRAM: &str = "Program";
pub const OP_DOCUMENT: &str = "Document";
pub const OP_CORE_DOCUMENT: &str = "CoreDocument";
pub const OP_TYPE_CONTEXT: &str = "TypeContext";
pub const OP_DIR_CAP: &str = "DirCap";
pub const OP_NET_CAP: &str = "NetCap";
pub const OP_HANDLE: &str = "Handle";
pub const OP_FILE: &str = "File";
pub const OP_BUILDER_HANDLE: &str = "BuilderHandle";
pub const OP_TASK: &str = "Task";
pub const OP_CHANNEL: &str = "Channel";
pub const OP_CONTEXT: &str = "Context";
pub const OP_REACTIVE_CELL: &str = "ReactiveCell";
pub const OP_CLOCK_CAP: &str = "ClockCap";
pub const OP_TIMEZONE: &str = "Timezone";
pub const OP_TIMESTAMP: &str = "Timestamp";
pub const OP_DURATION: &str = "Duration";
pub const OP_DECIMAL: &str = "Decimal";
pub const OP_BIG_INT: &str = "BigInt";
pub const OP_QUIC_SESSION: &str = "QuicSession";
pub const OP_QUIC_DATAGRAM_HANDLE: &str = "QuicDatagramHandle";
pub const OP_HTTP2_SESSION: &str = "Http2Session";
pub const OP_HTTP3_SESSION: &str = "Http3Session";
pub const OP_URI: &str = "Uri";
pub const OP_URN: &str = "Urn";

// ── Dispatch tag constants ────────────────────────────────────────────────────
//
// The dispatch tag strings used in `type_to_dispatch_tag()` to find matching
// typeclass instance arms. These correspond to the type-stage alias names
// declared in `stdlib/builtin_core.llt` (e.g., `Integer: TypeNode.Int`).

pub const DISPATCH_INTEGER: &str = "Integer";
pub const DISPATCH_FLOAT: &str = "Float";
pub const DISPATCH_STRING: &str = "String";
pub const DISPATCH_BYTES: &str = "Bytes";

// ── Structural discharge constants ───────────────────────────────────────────
//
// The `structural:` field value in class declarations (e.g. in prelude.llt).
// Used in `typecheck.rs` to convert a class's structural hint to a
// `StructuralDischarge` enum. If the prelude changes this string, update only
// this constant.

pub const STRUCTURAL_CLOSED_DICT: &str = "closed-dict";

// ── Static display name arrays ──────────────────────────────────────────────
//
// Each array maps a ctor_index (used as array index) to the human-readable
// display name for that constructor. Arrays cover both declared (builtin_core.llt)
// and Rust-internal constructors.

pub const TYPEVALUE_CTOR_NAMES: &[&str] = &[
    "TypeValue.Repr",
    "TypeValue.Phantom",
    "TypeValue.Var",
    "TypeValue.Fn",
    "TypeValue.App",
    "TypeValue.Op",
    "TypeValue.Record",
    "TypeValue.Union",
    "TypeValue.Inter",
    "TypeValue.Neg",
    "TypeValue.Scheme",
    "TypeValue.IntLit",
    "TypeValue.FloatLit",
    "TypeValue.StrLit",
    "TypeValue.Recursive",
    "TypeValue.RecursiveRef",
    "TypeValue.NominalVariant",
    "TypeValue.Unknown",
    "TypeValue.Never",
    "TypeValue.Top",
    "TypeValue.StageApp",
    "TypeValue.Error",
    "TypeValue.Nominal",
];

pub const ROWTAIL_CTOR_NAMES: &[&str] = &["RowTail.Closed", "RowTail.Var", "RowTail.Uniform"];

pub const TYPENODE_CTOR_NAMES: &[&str] = &[
    "TypeNode.Int",
    "TypeNode.String",
    "TypeNode.Float",
    "TypeNode.Bytes",
    "TypeNode.Never",
    "TypeNode.Top",
    "TypeNode.Unknown",
    "TypeNode.Proxy",
    "TypeNode.Callable",
    "TypeNode.Absent",
    "TypeNode.Union",
    "TypeNode.Intersect",
    "TypeNode.Negation",
    "TypeNode.Arrow",
    "TypeNode.Recursive",
    "TypeNode.RecursiveRef",
    "TypeNode.Dict",
    "TypeNode.TypeConstructor",
    "TypeNode.TypeApplication",
    "TypeNode.TypeVar",
    "TypeNode.IntLiteral",
    "TypeNode.StringLiteral",
    "TypeNode.FloatLiteral",
];

pub const BOOL_CTOR_NAMES: &[&str] = &["true", "false"];

pub const PROGRAM_ITEM_CTOR_NAMES: &[&str] = &["ProgramItem.File", "ProgramItem.Expr"];

pub const EXPR_CTOR_NAMES: &[&str] = &[
    "Expression.Literal",
    "Expression.VarRef",
    "Expression.DotAccess",
    "Expression.Pipe",
    "Expression.Sequential",
    "Expression.Dict",
    "Expression.Call",
    "Expression.Fn",
    "Expression.TypeAlias",
    "Expression.TypeAssert",
    "Expression.Annotated",
    "Expression.Rest",
    "Expression.Match",
    "Expression.LetDecl",
    "Expression.CaseArm",
    "Expression.Quote",
    "Expression.Unquote",
    "Expression.UnquoteSplice",
    "Expression.Splice",
    "Expression.SyntaxClass",
    "Expression.ClassDecl",
    "Expression.InstanceDecl",
    "Expression.PatternDecl",
    "Expression.Placeholder",
    "Expression.Error",
    "Expression.Entry",
    "Expression.NamedArg",
    "Expression.Parameter",
];

pub const ENTRY_CTOR_NAMES: &[&str] = &["Entry.Keyed", "Entry.Unkeyed"];

pub const ANNOTATION_CTOR_NAMES: &[&str] = &[
    "Annotation.Simple",
    "Annotation.PropDict",
    "Annotation.Null",
    "Annotation.Annotated",
    "Annotation.Unknown",
];

pub const DOTKEY_CTOR_NAMES: &[&str] = &["DotKey.Ident", "DotKey.Index"];

// ── Display name lookup functions ───────────────────────────────────────────

/// Look up the display name for a constructor by `(type_decl_id, ctor_index)`.
///
/// Returns a static string for known builtin types. Returns `"<user-type>"`
/// for user-defined types whose display names require a TyConDef lookup.
pub fn ctor_display_name(type_decl_id: u64, ctor_index: u32) -> &'static str {
    use crate::value::*;
    let idx = ctor_index as usize;
    match type_decl_id {
        TYPE_ID_TYPE_VALUE => TYPEVALUE_CTOR_NAMES
            .get(idx)
            .copied()
            .unwrap_or("<unknown-tv>"),
        TYPE_ID_ROW_TAIL => ROWTAIL_CTOR_NAMES
            .get(idx)
            .copied()
            .unwrap_or("<unknown-rt>"),
        TYPE_ID_TYPE_NODE => TYPENODE_CTOR_NAMES
            .get(idx)
            .copied()
            .unwrap_or("<unknown-tn>"),
        TYPE_ID_BOOL => BOOL_CTOR_NAMES
            .get(idx)
            .copied()
            .unwrap_or("<unknown-bool>"),
        TYPE_ID_PROGRAM_ITEM => PROGRAM_ITEM_CTOR_NAMES
            .get(idx)
            .copied()
            .unwrap_or("<unknown-pi>"),
        TYPE_ID_EXPR => EXPR_CTOR_NAMES
            .get(idx)
            .copied()
            .unwrap_or("<unknown-expr>"),
        TYPE_ID_ENTRY => ENTRY_CTOR_NAMES
            .get(idx)
            .copied()
            .unwrap_or("<unknown-entry>"),
        TYPE_ID_ANNOTATION => ANNOTATION_CTOR_NAMES
            .get(idx)
            .copied()
            .unwrap_or("<unknown-ann>"),
        TYPE_ID_DOT_KEY => DOTKEY_CTOR_NAMES
            .get(idx)
            .copied()
            .unwrap_or("<unknown-dk>"),
        TYPE_ID_CONSTRAINT_DECL => "ConstraintDecl",
        TYPE_ID_VAR_DECL => "VarDecl",
        _ => "<user-type>",
    }
}

/// Look up the display name for a constructor, falling back to TyConDef lookup
/// for user-defined types. Returns a `String` since user-type names are owned.
pub fn ctor_display_name_full(
    type_decl_id: u64,
    ctor_index: u32,
    tycon_env: Option<&std::collections::HashMap<u64, std::sync::Arc<crate::type_def::TyConDef>>>,
) -> String {
    let static_name = ctor_display_name(type_decl_id, ctor_index);
    if static_name != "<user-type>" {
        return static_name.to_string();
    }
    if let Some(env) = tycon_env {
        if let Some(def) = env.get(&type_decl_id) {
            if let Some((bare_name, _arity)) = def.constructors.get(ctor_index as usize) {
                return bare_name.clone();
            }
        }
    }
    format!("Ctor({type_decl_id}.{ctor_index})")
}

// ── TypeValue inspection helpers ─────────────────────────────────────────────

/// Extract the constructor index from a TypeValue (`Value::Variant`).
///
/// Returns the `ctor_index` or `None` if the value is not a `Variant`.
pub fn typevalue_ctor_index(tv: &std::sync::Arc<crate::value::Value>) -> Option<u32> {
    match tv.as_ref() {
        crate::value::Value::Variant { ctor_index, .. } => Some(*ctor_index),
        _ => None,
    }
}
