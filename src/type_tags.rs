//! TypeValue constructor tag string constants.
//!
//! All TypeValue ctor strings used in Rust (in match arms, comparisons, and
//! construction sites) are defined here as named constants. This is the single
//! authoritative location — if the tinct-side TypeValue declaration changes a
//! constructor name, only this file needs to be updated.
//!
//! The canonical source of truth for these names is `stdlib/builtin_core.llt`
//! (the `TypeValue: [type ...]` declaration). Every constant in the
//! "declared in builtin_core.llt" section below must match the constructor
//! name in that declaration exactly. The "Rust-internal-only sentinels" section
//! is exempt from this invariant — those constants are matched defensively but
//! are NOT declared in builtin_core.llt.

// ── TypeValue constructors declared in builtin_core.llt ─────────────────────

pub const TV_VAR: &str = "TypeValue.Var";
pub const TV_FN: &str = "TypeValue.Fn";
pub const TV_RECORD: &str = "TypeValue.Record";
pub const TV_UNION: &str = "TypeValue.Union";
pub const TV_INTER: &str = "TypeValue.Inter";
pub const TV_NEG: &str = "TypeValue.Neg";
pub const TV_REPR: &str = "TypeValue.Repr";
pub const TV_OP: &str = "TypeValue.Op";
pub const TV_APP: &str = "TypeValue.App";
pub const TV_SCHEME: &str = "TypeValue.Scheme";
pub const TV_UNKNOWN: &str = "TypeValue.Unknown";
pub const TV_NEVER: &str = "TypeValue.Never";
pub const TV_TOP: &str = "TypeValue.Top";
pub const TV_INT_LIT: &str = "TypeValue.IntLit";
pub const TV_STR_LIT: &str = "TypeValue.StrLit";
pub const TV_FLOAT_LIT: &str = "TypeValue.FloatLit";
pub const TV_RECURSIVE: &str = "TypeValue.Recursive";
pub const TV_RECURSIVE_REF: &str = "TypeValue.RecursiveRef";
pub const TV_NOMINAL_VARIANT: &str = "TypeValue.NominalVariant";
pub const TV_PHANTOM: &str = "TypeValue.Phantom";

// ── Rust-internal-only TypeValue sentinels (not declared in builtin_core.llt) ──
//
// These constants are matched defensively in Rust code but are never constructed
// by normal type inference. If encountered, they indicate a TypeValue produced
// by an internal mechanism not yet exposed to user code. They are NOT part of
// the canonical-source invariant above.

pub const TV_STAGE_APP: &str = "TypeValue.StageApp";
pub const TV_ERROR: &str = "TypeValue.Error";

// ── RowTail constructors ─────────────────────────────────────────────────────

pub const RT_CLOSED: &str = "RowTail.Closed";
pub const RT_VAR: &str = "RowTail.Var";
pub const RT_UNIFORM: &str = "RowTail.Uniform";

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

// ── Repr discriminant strings ────────────────────────────────────────────────
//
// These strings identify Rust Value enum variants in the TypeValue.Repr payload.
// They correspond to the `repr:` values in builtin_core.llt and must match the
// Rust variant names exactly as used in the discriminant comparisons.

pub const REPR_INT: &str = "Value::Int";
pub const REPR_U64: &str = "Value::U64";
pub const REPR_FLOAT: &str = "Value::Float";
pub const REPR_STRING: &str = "Value::String";
pub const REPR_BYTES: &str = "Value::Bytes";
// Note: there is no REPR_BOOL constant. Value::Bool does not exist as a runtime
// variant — tinct booleans are `Variant { ctor: "true" | "false" }`. Bool has no
// valid TypeValue.Repr discriminant and must never appear in a TypeValue.Repr payload.
pub const REPR_DICT: &str = "Value::Dict";
pub const REPR_FUNCTION: &str = "Value::Function";
/// Not currently used in type matching — Value::Builtin maps to REPR_FUNCTION at runtime
/// (ground_typevalue_of treats both Function and Builtin as REPR_FUNCTION). Reserved for
/// future use if Builtin values need their own TypeValue.Repr discriminant.
pub const REPR_BUILTIN: &str = "Value::Builtin";
pub const REPR_DECIMAL: &str = "Value::Decimal";
pub const REPR_BIGINT: &str = "Value::BigInt";
pub const REPR_DIR_CAP: &str = "Value::DirCap";
pub const REPR_REVOCABLE_DIR_CAP: &str = "Value::RevocableDirCap";
pub const REPR_NET_CAP: &str = "Value::NetCap";
pub const REPR_FILE: &str = "Value::File";
pub const REPR_CLOCK_CAP: &str = "Value::ClockCap";
pub const REPR_TASK: &str = "Value::Task";
pub const REPR_CHANNEL: &str = "Value::Channel";
pub const REPR_CONTEXT: &str = "Value::Context";
pub const REPR_REACTIVE_CELL: &str = "Value::ReactiveCell";
pub const REPR_QUIC_SESSION: &str = "Value::QuicSession";
pub const REPR_QUIC_DATAGRAM_HANDLE: &str = "Value::QuicDatagramHandle";
pub const REPR_HTTP2_SESSION: &str = "Value::Http2Session";
pub const REPR_HTTP3_SESSION: &str = "Value::Http3Session";
pub const REPR_URI: &str = "Value::Uri";
pub const REPR_PROGRAM: &str = "Value::Program";
pub const REPR_DOCUMENT: &str = "Value::Document";
pub const REPR_TYPE_CONTEXT: &str = "Value::TypeContext";
pub const REPR_TIMESTAMP: &str = "Value::Timestamp";
pub const REPR_DURATION: &str = "Value::Duration";
pub const REPR_TIMEZONE: &str = "Value::Timezone";
pub const REPR_PROXY: &str = "Value::Proxy";
pub const REPR_VARIANT: &str = "Value::Variant";
pub const REPR_BROADCAST_CHANNEL: &str = "Value::BroadcastChannel";
pub const REPR_ONESHOT_SENDER: &str = "Value::OneshotSender";
pub const REPR_ONESHOT_RECEIVER: &str = "Value::OneshotReceiver";
pub const REPR_ARENA: &str = "Value::Arena";
pub const REPR_EXPRESSION: &str = "Value::Expression";
pub const REPR_ANNOTATED: &str = "Value::Annotated";
pub const REPR_CORE_DOCUMENT: &str = "Value::CoreDocument";

// ── TypeValue payload field name constants ───────────────────────────────────
//
// Canonical field names used in TypeValue payload dicts. Each must match the
// field name declared in `stdlib/builtin_core.llt`.

/// Field key for TypeValue.Var: { name: String }
pub const FIELD_NAME: &str = "name";
/// Field key for TypeValue.Repr: { repr: String, is: [or [] Fn] }
pub const FIELD_REPR: &str = "repr";
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

// ── ConstraintDecl constructor ───────────────────────────────────────────────
//
// ConstraintDecl is the tinct-side variant used to represent typeclass
// constraints in the inference engine. It is not part of TypeValue itself
// but is constructed and matched alongside TypeValues in type_class.rs and
// type_unify.rs. Centralised here so that a rename in builtin_core.llt only
// requires updating this one constant.

/// Constructor tag for a type-class constraint: `ConstraintDecl { class, args }`.
pub const TV_CONSTRAINT_DECL: &str = "ConstraintDecl";
/// Constructor tag for a type variable declaration in TypeValue.Scheme vars dict.
pub const TV_VAR_DECL: &str = "VarDecl";

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

// ── Tinct boolean variant ctors ──────────────────────────────────────────────
//
// Tinct booleans are represented as unit Variants: `Variant { ctor: "true" }`
// and `Variant { ctor: "false" }`. This encoding is defined in `stdlib/builtin_core.llt`
// as the protocol for Rust builtins that return boolean results (e.g., equality checks).
// These constants are the authoritative spelling — update them if `builtin_core.llt` changes.

/// Unit-variant ctor for tinct `true`.
pub const BOOL_TRUE: &str = "true";
/// Unit-variant ctor for tinct `false`.
pub const BOOL_FALSE: &str = "false";

// ── TypeNode / TypeValue tycon name prefixes ─────────────────────────────────

/// The tycon prefix used by TypeNode constructors (e.g. `"TypeNode.Int"`).
///
/// Used by `typenode_value_to_type` (the authorized TypeNode→TypeValue translator) and
/// `typevalue_to_typenode` (the inverse) to identify TypeNode values. No other Rust code
/// should branch on this prefix — route through the authorized translators instead.
pub const TYCON_TYPENODE: &str = "TypeNode";

/// The tycon prefix used by TypeValue constructors (e.g. `"TypeValue.Repr"`).
pub const TYCON_TYPEVALUE: &str = "TypeValue";

// ── TypeNode constructor tag constants ───────────────────────────────────────
//
// Fully-qualified constructor tags for TypeNode variants declared in
// `stdlib/builtin_core.llt`. Every `"TypeNode.X"` string literal that
// appears in Rust must use one of these constants — if a TypeNode constructor
// is renamed in builtin_core.llt, only this file needs to change.

// Primitive leaf types
pub const TN_INT: &str = "TypeNode.Int";
pub const TN_FLOAT: &str = "TypeNode.Float";
pub const TN_STRING: &str = "TypeNode.String";
pub const TN_BYTES: &str = "TypeNode.Bytes";
pub const TN_NEVER: &str = "TypeNode.Never";
pub const TN_UNKNOWN: &str = "TypeNode.Unknown";
pub const TN_TOP: &str = "TypeNode.Top";
pub const TN_ABSENT: &str = "TypeNode.Absent";
pub const TN_PROXY: &str = "TypeNode.Proxy";

// Composite / structural TypeNode constructors
pub const TN_UNION: &str = "TypeNode.Union";
pub const TN_INTERSECT: &str = "TypeNode.Intersect";
pub const TN_NEGATION: &str = "TypeNode.Negation";
pub const TN_DICT: &str = "TypeNode.Dict";
pub const TN_ARROW: &str = "TypeNode.Arrow";
pub const TN_CALLABLE: &str = "TypeNode.Callable";

// Kind-annotated TypeVar sentinel
pub const TN_TYPE_VAR: &str = "TypeNode.TypeVar";

// Opaque builtin types — each maps to TypeValue.Op
pub const TN_PROGRAM: &str = "TypeNode.Program";
pub const TN_DOCUMENT: &str = "TypeNode.Document";
pub const TN_CORE_DOCUMENT: &str = "TypeNode.CoreDocument";
pub const TN_TYPE_CONTEXT: &str = "TypeNode.TypeContext";
pub const TN_DIR_CAP: &str = "TypeNode.DirCap";
pub const TN_NET_CAP: &str = "TypeNode.NetCap";
pub const TN_HANDLE: &str = "TypeNode.Handle";
pub const TN_FILE: &str = "TypeNode.File";
pub const TN_BUILDER_HANDLE: &str = "TypeNode.BuilderHandle";
pub const TN_TASK: &str = "TypeNode.Task";
pub const TN_CHANNEL: &str = "TypeNode.Channel";
pub const TN_CONTEXT: &str = "TypeNode.Context";
pub const TN_REACTIVE_CELL: &str = "TypeNode.ReactiveCell";
pub const TN_CLOCK_CAP: &str = "TypeNode.ClockCap";
pub const TN_TIMEZONE: &str = "TypeNode.Timezone";
pub const TN_TIMESTAMP: &str = "TypeNode.Timestamp";
pub const TN_DURATION: &str = "TypeNode.Duration";
pub const TN_DECIMAL: &str = "TypeNode.Decimal";
pub const TN_BIG_INT: &str = "TypeNode.BigInt";
pub const TN_QUIC_SESSION: &str = "TypeNode.QuicSession";
pub const TN_QUIC_DATAGRAM_HANDLE: &str = "TypeNode.QuicDatagramHandle";
pub const TN_HTTP2_SESSION: &str = "TypeNode.Http2Session";
pub const TN_HTTP3_SESSION: &str = "TypeNode.Http3Session";
pub const TN_URI: &str = "TypeNode.Uri";
pub const TN_URN: &str = "TypeNode.Urn";

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

// ── TypeValue inspection helpers ─────────────────────────────────────────────
//
// Canonical implementations of TypeValue utility functions. Placing them here
// ensures all modules can import them via `use crate::type_tags::*` without
// creating circular dependencies — type_tags depends only on value.rs, which
// imports neither type_tags nor any inference module.

/// Extract the constructor tag from a TypeValue (`Value::Variant`).
///
/// Returns the bare ctor string (e.g., `"TypeValue.Union"`) or `None` if the
/// value is not a `Variant` (e.g., an `unknown_type_val` empty Dict).
///
/// This is the canonical definition. `type_class::typevalue_ctor` and
/// `type_infer::typevalue_ctor` both delegate to this function.
pub fn typevalue_ctor(tv: &std::sync::Arc<crate::value::Value>) -> Option<&str> {
    match tv.as_ref() {
        crate::value::Value::Variant { ctor, .. } => Some(ctor.as_ref()),
        _ => None,
    }
}
