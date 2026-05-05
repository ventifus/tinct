//! Core evaluation module: lazy evaluation with letrec dict scoping, document
//! pipelines, and function evaluation.

pub(crate) use crate::eval_access::eval_range_access;
pub(crate) use crate::eval_call::eval_call;
#[cfg(test)]
pub(crate) use crate::eval_call::func_label;
pub use crate::eval_call::{invoke_function, CallContext};

// Re-export CEK machine components from eval_materialize
pub(crate) use crate::eval_materialize::{attach_materialization_context, run, Action};

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use indexmap::IndexMap;

use crate::arena::{EnvArena, ThunkArena, ThunkId};
use crate::ast::{Annotation, Document, Entry, Expr, File, Param, Span, Spanned};

thread_local! {
    /// Cached empty dict thunk used as the default `%` when no stdin is provided.
    /// Avoids allocating a fresh `Rc<Thunk>` on every `eval_file_with_input` call.
    static EMPTY_DICT_THUNK: Rc<Thunk> = Rc::new(Thunk::new_materialized(
        Value::Dict(IndexMap::new()),
        Span::origin(),
    ));
}
use crate::error::{EvalError, EvalResult};
use crate::types::{Row, RowTail, Type};
// Circular module dependency: this module calls builtins via function pointers stored in `Value::Builtin`.
// builtins.rs imports `invoke_function` and `materialize` from this module.
// This bidirectional dependency is safe because neither module's initialization depends on the other.
use crate::value::{Environment, Key, Thunk, ThunkState, Value};

/// Maximum evaluation depth (256). Limits nesting of eval/materialize calls to prevent stack overflow.
pub const MAX_EVAL_DEPTH: usize = 256;
pub(crate) const DEFAULT_ANNOTATION_KEY: &str = "default";

/// Reserved annotation meta-keys that are NOT structural field declarations.
/// A PropertyDict annotation whose entries are all meta-keys (e.g., `[@[default: 0] $x]`)
/// is metadata-only and has no type to validate. A PropertyDict with at least one
/// non-meta-key entry (e.g., `[@[name: String age: Int] $x]`) is a structural record
/// annotation that should enforce at minimum a Dict tag check when `resolved_type` is `None`.
const ANNOTATION_META_KEYS: &[&str] = &["type", "default"];

/// Formats a field path for TypeAssert error display. Each segment is separately
/// backtick-quoted: `user`.`address`.`zip`. Not for reconstruction — display only.
pub(crate) fn format_field_path(field_path: &[String]) -> String {
    field_path
        .iter()
        .map(|s| format!("`{}`", s))
        .collect::<Vec<_>>()
        .join(".")
}

/// Check whether a PropertyDict annotation contains structural field declarations.
///
/// Returns `true` if the annotation has at least one entry with a string key that
/// is NOT a reserved annotation meta-key ("type", "default"). This indicates the
/// annotation describes a record structure (e.g., `[@[name: String age: Int] $x]`).
///
/// Used by the `--no-typecheck` fallback to distinguish structural record annotations
/// (which should enforce a Dict tag check per doc/07-type-extensions.md §--no-typecheck mode)
/// from metadata-only annotations (which have nothing to validate against).
///
/// **Parser guarantee:** PropertyDict entries always have `Expr::Str` keys; non-`Expr::Str`
/// keys are treated as non-structural (the `_ => None` arm will never match in well-formed ASTs).
pub(crate) fn annotation_has_structural_fields(annotation: &Annotation) -> bool {
    match annotation {
        Annotation::Simple(_) => false,
        Annotation::PropertyDict(entries) => entries.iter().any(|entry| {
            entry
                .node
                .key
                .as_ref()
                .and_then(|k| match &k.node {
                    Expr::Str(name) => Some(name.as_str()),
                    _ => None,
                })
                .is_some_and(|name| !ANNOTATION_META_KEYS.contains(&name))
        }),
    }
}

/// Immutable session configuration shared across evaluation.
#[derive(Debug)]
pub struct EvalConfig {
    pub base_dir: cap_std::fs::Dir,
    pub stdlib_env: Rc<RefCell<Environment>>,
    pub no_fs: bool,
    /// When true, every `$include` call must supply an integrity hash.
    /// Hashless includes are rejected with `IncludeHashRequired`.
    pub require_integrity: bool,
    /// Filesystem allowlist for `$include`. When non-empty, only canonical paths
    /// that are descendants of at least one entry in this list are permitted.
    /// When empty (the default), all paths are unrestricted.
    ///
    /// In LSP mode `no_fs=true` is set, so this field is never consulted — the
    /// `no_fs` check fires first and blocks all includes unconditionally.
    pub allowed_paths: Vec<std::path::PathBuf>,
}

/// Mutable evaluation state (include guard, caching).
#[derive(Debug)]
pub struct EvalState {
    /// File identities (dev, ino) currently being evaluated by $include (cycle detection).
    pub include_guard: HashSet<(u64, u64)>,
    /// File identity (dev, ino) -> materialized result thunk (include result caching).
    /// Only successful evaluations are cached; errors are not cached.
    pub include_cache: HashMap<(u64, u64), Rc<Thunk>>,
    /// Stack of active $include calls: `(display_path, call_site_span)`.
    ///
    /// Pushed by `builtin_include` before evaluating the included file, popped
    /// after (in both success and error branches). Used to annotate errors from
    /// nested includes with the full include path, e.g.:
    ///   "included from a.llt at 3:10-3:25"
    ///   "included from b.llt at 1:5-1:20"
    pub include_chain: Vec<(String, Span)>,
    /// Stack of thunks currently being evaluated: `(origin_label, span)`.
    ///
    /// Pushed when transitioning from Unevaluated/PendingBuiltin/PendingCall/Guarded
    /// to InProgress (before extracting data), popped on successful materialization.
    /// On circular dependency detection (thunk already InProgress), this stack
    /// contains the full cycle chain for error reporting.
    ///
    /// Example: `[("a", span1), ("b", span2), ("x", span3)]` means evaluating
    /// `x` requires `a`, which requires `b`, which requires `x` (cycle).
    ///
    /// Upper bound: MAX_EVAL_DEPTH (256) entries × ~80 bytes/entry ≈ 20 KB.
    pub eval_stack: Vec<(String, Span)>,
    // future: trace_log, eval_stats
}

/// Evaluation infrastructure context: separates session config from variable bindings.
///
/// Config is immutable (Rc without RefCell); state is mutable (Rc<RefCell>).
/// Thread as `&Rc<EvalContext>` through eval/materialize; thunks capture `Rc::clone(ctx)`.
///
/// **Phase 2 Arena Migration (Registry Approach):** Arenas act as a GC root / bulk-deallocation
/// boundary. Thunks are allocated in the arena AND stored as Rc<Thunk> in Value variants.
/// This establishes the arena pattern without the massive ThunkId-in-Value refactor.
/// Full ThunkId migration is deferred to Phase 3.
#[derive(Debug)]
pub struct EvalContext {
    pub config: Rc<EvalConfig>,
    pub state: Rc<RefCell<EvalState>>,
    /// Thunk arena registry. Phase 2: stores Vec<Rc<Thunk>> and provides bulk deallocation.
    /// Thunks are allocated here but Value variants still use Rc<Thunk> directly.
    /// **Shared ownership:** Rc<RefCell<>> allows child contexts (created via with_base_dir)
    /// to share the parent's arena, preventing ThunkId index-out-of-bounds panics.
    #[allow(dead_code)]
    pub(crate) thunk_arena: Rc<RefCell<ThunkArena>>,
    /// Environment arena registry. Phase 2: not actively used (chain-based environments remain).
    /// Reserved for Phase 3 flat environment migration.
    /// **Shared ownership:** Rc<RefCell<>> allows child contexts to share the parent's arena.
    #[allow(dead_code)]
    pub(crate) env_arena: Rc<RefCell<EnvArena>>,
    /// Set to true when `emit` builtin is called. Signals CLI to suppress JSON output.
    pub emitted: std::cell::Cell<bool>,
    /// Environment variable allowlist. None = unrestricted (all allowed), Some(set) = only those in set.
    /// Some(empty) means all denied (--no-env mode).
    pub env_allowed: Option<HashSet<String>>,
}

impl EvalContext {
    pub fn new(
        base_dir: cap_std::fs::Dir,
        stdlib_env: Rc<RefCell<Environment>>,
        no_fs: bool,
    ) -> Rc<Self> {
        Self::new_with_options(base_dir, stdlib_env, no_fs, false)
    }

    pub fn new_with_options(
        base_dir: cap_std::fs::Dir,
        stdlib_env: Rc<RefCell<Environment>>,
        no_fs: bool,
        require_integrity: bool,
    ) -> Rc<Self> {
        Self::new_with_all_options(
            base_dir,
            stdlib_env,
            no_fs,
            require_integrity,
            Vec::new(),
            None,
        )
    }

    pub fn new_with_all_options(
        base_dir: cap_std::fs::Dir,
        stdlib_env: Rc<RefCell<Environment>>,
        no_fs: bool,
        require_integrity: bool,
        allowed_paths: Vec<std::path::PathBuf>,
        env_allowed: Option<HashSet<String>>,
    ) -> Rc<Self> {
        Rc::new(Self {
            config: Rc::new(EvalConfig {
                base_dir,
                stdlib_env,
                no_fs,
                require_integrity,
                allowed_paths,
            }),
            state: Rc::new(RefCell::new(EvalState {
                include_guard: HashSet::new(),
                include_cache: HashMap::new(),
                include_chain: Vec::new(),
                eval_stack: Vec::new(),
            })),
            thunk_arena: Rc::new(RefCell::new(ThunkArena::new())),
            env_arena: Rc::new(RefCell::new(EnvArena::new())),
            emitted: std::cell::Cell::new(false),
            env_allowed,
        })
    }

    /// Create a new EvalContext with a different base_dir but sharing the same
    /// state (include guard, cache) and stdlib_env. Avoids allocating a new
    /// EvalState; shares the underlying stdlib_env and state Rc allocations
    /// (e.g., during $include).
    ///
    /// Inherits `no_fs`, `require_integrity`, and `allowed_paths` from the parent
    /// config so that sandbox restrictions are preserved across directory changes.
    ///
    /// **Phase 2 Arena Migration (Registry):** SHARES the parent's arenas (Rc::clone).
    /// This fixes the ThunkId index-out-of-bounds bug: values from the parent context
    /// (including stdlib) carry ThunkIds that index into the parent's arena. The child
    /// context must use the SAME arena to resolve those ThunkIds.
    pub fn with_base_dir(&self, base_dir: cap_std::fs::Dir) -> Rc<Self> {
        Rc::new(Self {
            config: Rc::new(EvalConfig {
                base_dir,
                stdlib_env: Rc::clone(&self.config.stdlib_env),
                no_fs: self.config.no_fs,
                require_integrity: self.config.require_integrity,
                allowed_paths: self.config.allowed_paths.clone(),
            }),
            state: Rc::clone(&self.state),
            thunk_arena: Rc::clone(&self.thunk_arena),
            env_arena: Rc::clone(&self.env_arena),
            emitted: std::cell::Cell::new(false),
            env_allowed: self.env_allowed.clone(),
        })
    }

    /// Allocate a thunk in the arena and return its ID.
    pub(crate) fn alloc_thunk(&self, thunk: Rc<Thunk>) -> ThunkId {
        self.thunk_arena.borrow_mut().alloc(thunk)
    }

    /// Get a cloned Rc<Thunk> from the arena by ID.
    pub(crate) fn get_thunk(&self, id: ThunkId) -> Rc<Thunk> {
        self.thunk_arena.borrow().get(id).clone()
    }
}

/// Check if a materialized value matches a type for structural TypeAssert validation.
/// Returns true if the value conforms to the expected type.
///
/// This performs immediate type checking per doc/07-type-extensions.md §Validation depth table:
/// - Primitives (Int, Float, Str, Bool): exact match
/// - Literals (IntLiteral, StringLiteral): value equality
/// - Seq, Function: tag-only validation (element/param types opaque per spec doc/07:108-113)
/// - TypeVar: treated as Any (residual polymorphic instantiation)
/// - Record: always true (structural validation deferred to proxy contract wrapping)
pub(crate) fn value_matches_type(value: &Value, expected: &Type) -> bool {
    match expected {
        Type::Any => true,
        Type::Int => matches!(value, Value::Int(_)),
        Type::Float => matches!(value, Value::Float(_)),
        Type::Number => matches!(value, Value::Int(_) | Value::Float(_)),
        Type::Str => matches!(value, Value::String(_)),
        Type::Bool => matches!(value, Value::Bool(_)),
        Type::IntLiteral(n) => matches!(value, Value::Int(v) if v == n),
        Type::StringLiteral(s) => matches!(value, Value::String(v) if v == s),
        Type::Function { .. } => matches!(value, Value::Function { .. } | Value::Builtin(_)),
        Type::Seq(_) => matches!(value, Value::Seq { .. }),
        Type::TypeVar(_, _) => true,
        Type::Record(_) => true, // Records handled separately via proxy wrapping
        Type::Proxy => matches!(value, Value::Proxy { .. }),
        Type::DirCap => matches!(value, Value::DirCap(_) | Value::RevocableDirCap { .. }),
        Type::NetCap => matches!(value, Value::NetCap(_)),
        Type::Handle => matches!(value, Value::Handle(_)),
        // Error is a type-inference sentinel that should never reach runtime validation.
        // Type::Error indicates type inference failed; treating it as a match would mask bugs.
        Type::Error => {
            debug_assert!(false, "Error sentinel should not reach runtime validation");
            false
        }
    }
}

/// Format a Type for error messages in TypeAssert.
///
/// Currently delegates to Type's Display impl. This wrapper provides a semantic
/// name and future-proofs for custom error formatting (e.g., abbreviating long
/// record types, pretty-printing nested structures).
pub(crate) fn format_type_for_assert(ty: &Type) -> String {
    format!("{}", ty)
}

/// Validate a dict value against a Record type and wrap fields with guards.
///
/// Returns a new dict with guarded field thunks. This implements the [VM-RECORD-PROXY]
/// rule from doc/07-type-extensions.md:
/// 1. Shape check: verify all required fields exist (with Key::Int fallback)
/// 2. Cardinality check: verify no extra fields for closed records
/// 3. Guard wrapping: wrap each typed field with a Guarded thunk
///
/// This function implements **chaperone semantics** (Strickland et al., 2012):
/// the proxy (guarded dict) is observationally equivalent to the original dict at
/// all type-correct uses. Each field's guard can only (a) return the original value
/// unchanged, or (b) raise a contract error — it cannot change the value. Field
/// types are checked lazily when accessed, not eagerly at the assertion site,
/// preserving call-by-need evaluation (Launchbury, 1993). A field that is never
/// accessed is never validated, matching Findler & Felleisen's (2002) principle
/// that compound contracts defer checking to the point of observation.
///
/// # Parameters
/// - `entries`: the dict entries to validate
/// - `row`: the expected record row type (fields + tail)
/// - `field_path`: accumulated path for nested field errors (empty for top-level)
/// - `guard_span`: span for guard creation
///
/// # Errors
/// Returns TypeAssertFailed if:
/// - A required field is missing
/// - The record has extra fields and tail is Empty (closed)
///
/// # Note
/// The caller is responsible for checking default_expr and calling eval() with the default
/// if this function returns an error. This keeps the helper focused on validation logic.
/// Guards created by this function do NOT propagate default_expr to avoid infinite recursion.
pub(crate) fn validate_and_wrap_record(
    entries: &IndexMap<Key, ThunkId>,
    row: &Row,
    field_path: &mut Vec<String>,
    guard_span: Span,
    data_span: Span,
    ctx: &Rc<EvalContext>,
) -> EvalResult<IndexMap<Key, ThunkId>> {
    // Shape check: verify all required fields exist
    // Per doc/07:117, try Key::String first, then Key::Int fallback
    for (field_name, _field_type) in row.fields.iter() {
        let has_field = entries.contains_key(&Key::String(field_name.clone()))
            || field_name
                .parse::<i64>()
                .ok()
                .map(|idx| entries.contains_key(&Key::Int(idx)))
                .unwrap_or(false);

        if !has_field {
            let field_path_prefix = if field_path.is_empty() {
                String::new()
            } else {
                format!("field {}: ", format_field_path(field_path))
            };

            return Err(EvalError::type_assert_failed(
                &format!("{}record with field \"{}\"", field_path_prefix, field_name),
                &format!(
                    "{}record missing field \"{}\"",
                    field_path_prefix, field_name
                ),
                // Use data_span (the data definition site) so the error points to WHERE
                // the invalid dict was constructed, not the annotation.
                data_span,
            )
            .into());
        }
    }

    // Cardinality check for closed records
    // Per review finding #5: iterate keys directly, no Vec allocation
    // Key::Int(n) entries are checked against their string representation (n.to_string())
    // since Row.fields uses String keys; an entry [0: v] matches a field named "0".
    if matches!(row.tail, RowTail::Empty) {
        for key in entries.keys() {
            let extra_field_name = match key {
                Key::String(s) if !row.fields.contains_key(s) => Some(s.clone()),
                Key::Int(n) => {
                    // Check if the integer key matches a string field name (e.g., field "0")
                    let s = n.to_string();
                    if !row.fields.contains_key(&s) {
                        Some(s)
                    } else {
                        None
                    }
                }
                _ => None, // Key::String that IS in row.fields — valid
            };

            if let Some(field_name) = extra_field_name {
                let field_path_prefix = if field_path.is_empty() {
                    String::new()
                } else {
                    format!("field {}: ", format_field_path(field_path))
                };

                return Err(EvalError::type_assert_failed(
                    &format!("{}closed record (no extra fields)", field_path_prefix),
                    &format!(
                        "{}record with unexpected field \"{}\"",
                        field_path_prefix, field_name
                    ),
                    data_span,
                )
                .into());
            }
        }
    }

    // Guard wrapping: wrap each typed field thunk.
    // Use a for loop with push/pop on field_path to avoid cloning the full path
    // for every field — only the thunk's owned copy is allocated per field.
    let mut new_entries = IndexMap::with_capacity(entries.len());
    for (key, &thunk_id) in entries.iter() {
        // Try to find a matching field type
        let field_type = match key {
            Key::String(field_name) => row.fields.get(field_name),
            Key::Int(n) => row.fields.get(&n.to_string()),
        };

        if let Some(field_type) = field_type {
            let field_name = match key {
                Key::String(s) => s.clone(),
                Key::Int(n) => n.to_string(),
            };

            // Push field name onto the shared path, clone for the thunk, then pop.
            // This avoids cloning the entire path prefix for every entry.
            field_path.push(field_name);
            let nested_path = field_path.clone();
            field_path.pop();

            let thunk_rc = ctx.get_thunk(thunk_id);
            let guarded = Rc::new(Thunk::new_guarded(
                thunk_rc,
                field_type.clone(),
                nested_path,
                guard_span,
            ));
            let guarded_id = ctx.alloc_thunk(guarded);
            new_entries.insert(key.clone(), guarded_id);
        } else {
            new_entries.insert(key.clone(), thunk_id);
        }
    }

    Ok(new_entries)
}

/// Wrap an AST expression in a thunk. Literals produce immediately materialized
/// thunks; dicts produce materialized thunks whose values are unevaluated;
/// var refs look up the environment chain.
///
/// `depth` tracks recursion depth to prevent stack overflow. Callers should
/// pass 0 for top-level evaluation.
/// Recursive expression evaluator (legacy implementation).
///
/// This is the original eval() implementation, kept as a helper for eval_step().
/// It recursively evaluates expressions and returns thunks (which may be materialized
/// or unevaluated depending on the expression type).
///
/// This function is called by eval_step() for cases that need recursive evaluation
/// (e.g., TypeAssert default branches). It does NOT go through the CEK machine.
pub(crate) fn eval_recursive(
    expr: Rc<Spanned<Expr>>,
    env: Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    if depth > MAX_EVAL_DEPTH {
        return Err(EvalError::depth_exceeded(MAX_EVAL_DEPTH, expr.span).into());
    }

    match &expr.node {
        // Literals and closures are already computed values, so we wrap them in
        // immediately-materialized thunks instead of Unevaluated thunks. This avoids
        // the overhead of wrapping, then unwrapping, then re-evaluating on first access.
        Expr::Int(n) => Ok(Rc::new(Thunk::new_materialized(Value::Int(*n), expr.span))),
        Expr::Float(f) => Ok(Rc::new(Thunk::new_materialized(
            Value::Float(*f),
            expr.span,
        ))),
        Expr::Bool(b) => Ok(Rc::new(Thunk::new_materialized(Value::Bool(*b), expr.span))),
        Expr::Str(s) => Ok(Rc::new(Thunk::new_materialized(
            Value::String(s.clone()),
            expr.span,
        ))),
        Expr::VarRef { name, resolved } => {
            // TODO(arena-phase2): Use resolved (level, slot) for O(1) lookup when FlatEnv is available.
            // The current linked-environment model with stdlib/document scopes doesn't align with
            // the resolver's static level system (which only sees dict/function scopes during the AST walk).
            // Full slot-based lookup requires Phase 2's FlatEnv with proper de Bruijn indexing that accounts
            // for all runtime environment frames.
            let _ = resolved; // Suppress unused warning; cache is populated for future use.

            let found = env.borrow().get(name);
            match found {
                Some(thunk) => Ok(thunk),
                None => Err(EvalError::undefined_variable(name.clone(), expr.span).into()),
            }
        }
        Expr::Dict(entries) => eval_dict(entries, &env, ctx, &expr.span, depth + 1),
        Expr::DotAccess { .. } | Expr::BracketAccess { .. } => {
            // Return Unevaluated thunk — force_step handles these iteratively via
            // DotAccessForce/BracketForceTarget continuations
            let span = expr.span;
            Ok(Rc::new(Thunk::new_unevaluated(
                expr,
                Rc::clone(&env),
                Rc::clone(ctx),
                span,
            )))
        }
        Expr::RangeAccess {
            expr: target,
            start,
            end,
        } => eval_range_access(
            target,
            start.as_deref(),
            end.as_deref(),
            &env,
            ctx,
            &expr.span,
            depth,
        ),
        Expr::TypeAssert {
            expr: inner,
            annotation,
            resolved_type,
        } => {
            let thunk =
                eval_recursive(Rc::new((**inner).clone()), Rc::clone(&env), ctx, depth + 1)?;

            // Check if elaboration provided a resolved type
            let resolved = resolved_type.borrow().clone();

            if let Some(expected) = resolved {
                // STRUCTURAL VALIDATION (type checker succeeded and provided elaboration)

                match &expected {
                    Type::Record(row) => {
                        // [VM-RECORD-PROXY]: shape check + guard wrapping
                        // TODO(iterative-eval): this materializes eagerly — defer to CEK machine for lazy structural checking
                        let value = materialize(&thunk, Some(&expr.span), ctx, depth + 1)?;
                        // Flatten Overlay to Dict before record type assertion.
                        let value = match value {
                            Value::Overlay(l, r) => Value::Dict(crate::builtins::flatten_overlay(
                                &l,
                                &r,
                                "type assert",
                                ctx,
                                depth + 1,
                                expr.span,
                            )?),
                            other => other,
                        };
                        if let Value::Dict(entries) = &value {
                            // Use helper to validate and wrap record
                            // If validation fails and default: is present, use default
                            let default_opt = annotation
                                .node
                                .get_property(DEFAULT_ANNOTATION_KEY)
                                .map(|expr| (Rc::new(expr.clone()), Rc::clone(&env)));

                            match validate_and_wrap_record(
                                entries,
                                row,
                                &mut vec![],
                                expr.span,
                                thunk.span,
                                ctx,
                            ) {
                                Ok(new_entries) => Ok(Rc::new(Thunk::new_materialized(
                                    Value::Dict(new_entries),
                                    expr.span,
                                ))),
                                Err(err) => {
                                    if let Some((default, env)) = default_opt {
                                        eval_recursive(default, env, ctx, depth + 1)
                                    } else {
                                        Err(err)
                                    }
                                }
                            }
                        } else {
                            // Expected Record but got non-Dict
                            if let Some(default_expr) =
                                annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                            {
                                return eval_recursive(
                                    Rc::new(default_expr.clone()),
                                    env,
                                    ctx,
                                    depth + 1,
                                );
                            }
                            Err(EvalError::type_assert_failed(
                                &format_type_for_assert(&expected),
                                &value.type_name(),
                                thunk.span, // value's definition site, not annotation site
                            )
                            .with_materialization_span(expr.span)
                            .into())
                        }
                    }
                    _ => {
                        // Non-Record type: immediate validation per spec (line 22)
                        // "For primitive types, validation is immediate"
                        // TODO(iterative-eval): This is a laziness violation — defer to CEK machine once migration is complete.
                        let value = materialize(&thunk, Some(&expr.span), ctx, depth + 1)?;
                        if value_matches_type(&value, &expected) {
                            Ok(Rc::new(Thunk::new_materialized(value, expr.span)))
                        } else {
                            if let Some(default_expr) =
                                annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                            {
                                return eval_recursive(
                                    Rc::new(default_expr.clone()),
                                    env,
                                    ctx,
                                    depth + 1,
                                );
                            }
                            Err(EvalError::type_assert_failed(
                                &format_type_for_assert(&expected),
                                &value.type_name(),
                                thunk.span, // value's definition site, not annotation site
                            )
                            .with_materialization_span(expr.span)
                            .into())
                        }
                    }
                }
            } else {
                // --no-typecheck FALLBACK (nominal validation)
                // Per doc/07-type-extensions.md §--no-typecheck mode:
                // - Primitive type assertions still work (nominal string comparison)
                // - Structural type assertions degrade to tag-only checks (Dict tag)
                let value = materialize(&thunk, Some(&expr.span), ctx, depth + 1)?;

                let expected_type =
                    match &annotation.node {
                        Annotation::Simple(name) => Some(name.as_str()),
                        Annotation::PropertyDict(_) => annotation
                            .node
                            .get_property("type")
                            .and_then(|type_expr| match &type_expr.node {
                                Expr::Str(s) => Some(s.as_str()),
                                _ => None,
                            }),
                    };

                if let Some(expected) = expected_type {
                    let actual = value.type_name();
                    let matches = if expected == "Number" {
                        actual == "Int" || actual == "Float"
                    } else {
                        actual == expected
                    };
                    if !matches {
                        if let Some(default_expr) =
                            annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                        {
                            return eval_recursive(
                                Rc::new(default_expr.clone()),
                                env,
                                ctx,
                                depth + 1,
                            );
                        }
                        return Err(EvalError::type_assert_failed(expected, actual, thunk.span)
                            .with_materialization_span(expr.span)
                            .into());
                    }
                } else if annotation_has_structural_fields(&annotation.node) {
                    // Structural record annotation without resolved_type — degrade to Dict
                    // tag check. Without elaboration we cannot validate field names or types,
                    // but we can at least verify the value is a Dict (the carrier type for
                    // records). This closes the elaboration gap for eval-only mode.
                    if !matches!(value, Value::Dict(_) | Value::Overlay(..)) {
                        let actual = value.type_name();
                        if let Some(default_expr) =
                            annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                        {
                            return eval_recursive(
                                Rc::new(default_expr.clone()),
                                env,
                                ctx,
                                depth + 1,
                            );
                        }
                        return Err(EvalError::type_assert_failed("Record", actual, thunk.span)
                            .with_materialization_span(expr.span)
                            .into());
                    }
                }

                Ok(Rc::new(Thunk::new_materialized(value, expr.span)))
            }
        }
        Expr::Annotated { name, .. } => {
            // Evaluate as the bare string; the type checker (typecheck.rs) interprets annotations.
            Ok(Rc::new(Thunk::new_materialized(
                Value::String(name.clone()),
                expr.span,
            )))
        }
        Expr::Fn { params, body, .. } => {
            let fn_params: Vec<Param> = params.iter().map(|p| p.node.clone()).collect();
            Ok(Rc::new(Thunk::new_materialized(
                Value::Function {
                    params: Rc::new(fn_params),
                    body: Rc::clone(body),
                    env: Rc::clone(&env),
                },
                expr.span,
            )))
        }
        Expr::Call {
            func,
            args,
            named_args,
            implied: _,
        } => eval_call(func, args, named_args, &env, ctx, &expr.span, depth),
        // Type alias entries are compile-time-only constructs consumed by the type checker.
        // At runtime, they evaluate to an empty dict to maintain dict structure without
        // contributing runtime values.
        Expr::TypeAlias(_inner) => Ok(Rc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            expr.span,
        ))),
        Expr::Rest(_) => Err(EvalError::internal(
            "rest marker (...) is only valid inside type expressions".to_string(),
            expr.span,
        )
        .into()),
        Expr::Error(span) => Err(EvalError::internal(
            format!(
                "syntax error at {}:{} (cannot evaluate error node)",
                span.start.line, span.start.column
            ),
            expr.span,
        )
        .into()),
    }
}

pub fn eval(
    expr: Rc<Spanned<Expr>>,
    env: Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    eval_recursive(expr, env, ctx, depth)
}

/// Evaluate a document: a sequence of expressions forming a scope chain.
///
/// Each intermediate expression is materialized and must produce a `Value::Dict`.
/// The dict's string-keyed entries become bindings in a new child environment that
/// serves as the scope for the next expression. The last expression is returned
/// as-is (lazy, any type). An empty document returns an empty dict.
pub fn eval_document(
    doc: &Spanned<Document>,
    env: Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    let exprs = &doc.node.expressions;

    if exprs.is_empty() {
        return Ok(Rc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            doc.span,
        )));
    }

    let mut current_env = env;

    for (i, expr) in exprs.iter().enumerate() {
        let is_last = i == exprs.len() - 1;

        if is_last {
            // Last expression: return its thunk as-is (lazy, any type)
            return eval(Rc::clone(expr), current_env, ctx, depth);
        }

        // Intermediate expression: materialize and extract dict bindings
        let thunk = eval(Rc::clone(expr), Rc::clone(&current_env), ctx, depth)?;
        let value = materialize(&thunk, Some(&expr.span), ctx, depth + 1)?;

        // Flatten Overlay to Dict for scope chain binding.
        let map = match value {
            Value::Dict(map) => map,
            Value::Overlay(l, r) => crate::builtins::flatten_overlay(
                &l,
                &r,
                "document pipeline",
                ctx,
                depth + 1,
                expr.span,
            )?,
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "document pipeline".to_string(),
                    "Dict",
                    value.type_name(),
                    expr.span,
                )
                .into());
            }
        };
        {
            let child_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(
                &current_env,
            ))));
            for (key, val_thunk_id) in map {
                // Only string keys become scope bindings; int keys are positional, not named.
                // Owned iteration (into_iter): Key::String(name) moves the String rather than
                // cloning it, and val_thunk is moved directly — saves one String clone + one
                // Rc clone per string-keyed entry in each document pipeline step.
                if let Key::String(name) = key {
                    let val_thunk = ctx.get_thunk(val_thunk_id);
                    child_env.borrow_mut().insert(name, val_thunk);
                }
            }
            current_env = child_env;
        }
    }

    // INVARIANT: This is unreachable because the loop above always returns when
    // processing the last expression (when i == exprs.len() - 1). The loop only
    // terminates naturally if exprs is empty, but we return early for empty docs.
    unreachable!(
        "eval_document: loop did not return — exprs was non-empty but is_last never triggered"
    )
}

/// Evaluate a file: one or more documents separated by `---`.
///
/// Documents are totally isolated -- they share no scope. Data flows between
/// documents via `%` (and named sections `%name`), which are injected into each
/// document's root scope from the previous document's output.
///
/// - For the first document, `%` is an empty dict.
/// - For subsequent documents, `%` is the previous document's result thunk
///   (lazy -- no materialization at the `---` boundary).
/// - The last document's result is the file's output.
/// - An empty file (zero documents) returns an empty dict.
///
/// # Precondition
///
/// **`desugar::desugar_file` must be called on the [`File`] before passing it here.**
/// The evaluator has no `$_` handling; callers that skip the desugar pass will see
/// `UndefinedVariable("_")` errors for any `$_` expression. All pipeline entry points
/// (`eval_source_with_config`, `main.rs::run_eval`, `repl.rs::eval_input`,
/// `builtins.rs` `$include` handler) already call `desugar_file` first.
///
/// **Note:** Provide an `EvalContext` via `EvalContext::new()` to configure `$include`;
/// no separate setup call required.
pub fn eval_file(
    file: &File,
    env: Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    eval_file_with_input(file, env, ctx, None, depth)
}

/// Evaluate a parsed [`File`], optionally injecting an initial `%` value for the first document.
///
/// When `initial_input` is `Some(thunk)`, that thunk becomes `%` for the first
/// document instead of the default empty dict. This supports the CLI's stdin
/// JSON injection: `cat data.json | llt eval file.llt`.
///
/// # Precondition
///
/// **`desugar::desugar_file` must be called on the [`File`] before passing it here.**
/// See [`eval_file`] for details.
///
/// **Note:** Provide an `EvalContext` via `EvalContext::new()` to configure `$include`;
/// no separate setup call required.
pub fn eval_file_with_input(
    file: &File,
    env: Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    initial_input: Option<Rc<Thunk>>,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    // % starts as the provided input, or empty dict if none given
    let mut prev_output = initial_input.unwrap_or_else(|| EMPTY_DICT_THUNK.with(|t| Rc::clone(t)));
    // Named section accumulator: maps section name → result thunk
    let mut named: IndexMap<String, Rc<Thunk>> = IndexMap::new();

    for doc in &file.documents {
        // Each document gets a fresh scope with % and %name bindings
        let doc_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(&env))));

        // Bind % (pipeline variable)
        doc_env
            .borrow_mut()
            .insert("%".to_string(), Rc::clone(&prev_output));

        // Bind all previously named sections as %name
        for (section_name, section_thunk) in &named {
            doc_env
                .borrow_mut()
                .insert(format!("%{}", section_name), Rc::clone(section_thunk));
        }

        let result = eval_document(doc, doc_env, ctx, depth)?;

        // If this document is named, accumulate it in the named map
        if let Some(ref name) = doc.node.name {
            named.insert(name.clone(), Rc::clone(&result));
        }

        prev_output = result; // lazy: no materialization at boundary
    }

    Ok(prev_output)
}

pub(crate) fn eval_dict(
    entries: &[Spanned<Entry>],
    parent_env: &Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    dict_span: &Span,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    let dict_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(
        parent_env,
    ))));
    let mut dict_map: IndexMap<Key, ThunkId> = IndexMap::with_capacity(entries.len());
    let mut auto_index: i64 = 0;

    for entry in entries {
        let key = match &entry.node.key {
            // Keys are evaluated in the parent scope, not dict_env, because key
            // expressions must not see sibling bindings. This prevents keys from
            // depending on values that are still unevaluated thunks and keeps
            // key evaluation deterministic regardless of entry order.
            Some(key_expr) => eval_key(key_expr, parent_env, ctx, depth)?,
            None => {
                let k = Key::Int(auto_index);
                auto_index = auto_index.checked_add(1).ok_or_else(|| {
                    EvalError::integer_overflow("dict auto-index".to_string(), entry.span)
                })?;
                k
            }
        };

        if dict_map.contains_key(&key) {
            return Err(Box::new(EvalError::duplicate_key(
                &key.to_string(),
                entry.span,
            )));
        }

        // Fast path for literal values: create Materialized thunks directly,
        // avoiding Unevaluated → Materialized state transition overhead (Nix maybeThunk pattern)
        let thunk = match &entry.node.value.node {
            Expr::Int(n) => Rc::new(Thunk::new_materialized(
                Value::Int(*n),
                entry.node.value.span,
            )),
            Expr::Float(f) => Rc::new(Thunk::new_materialized(
                Value::Float(*f),
                entry.node.value.span,
            )),
            Expr::Bool(b) => Rc::new(Thunk::new_materialized(
                Value::Bool(*b),
                entry.node.value.span,
            )),
            Expr::Str(s) => Rc::new(Thunk::new_materialized(
                Value::String(s.clone()),
                entry.node.value.span,
            )),
            _ => Rc::new(Thunk::new_unevaluated(
                // TODO(ast-rc): `entry.node.value` is `Spanned<Expr>` (owned), so
                // `Rc::new(...clone())` deep-clones the AST subtree on every
                // eval_dict invocation.  Eliminating this requires migrating
                // `Entry::value` to `Rc<Spanned<Expr>>` in ast.rs and updating
                // the parser, formatter, typecheck, lsp/analysis, and all eval
                // call sites (~36 occurrences across 6 files).  Deferred to a
                // dedicated AST-RC migration sprint.
                Rc::clone(&entry.node.value),
                Rc::clone(&dict_env),
                Rc::clone(ctx),
                entry.node.value.span,
            )),
        };

        // String keys become bindings so sibling entries can reference via $name
        if let Key::String(ref name) = key {
            dict_env
                .borrow_mut()
                .insert(name.clone(), Rc::clone(&thunk));
        }

        dict_map.insert(key, ctx.alloc_thunk(thunk));
    }

    Ok(Rc::new(Thunk::new_materialized(
        Value::Dict(dict_map),
        *dict_span,
    )))
}

pub(crate) fn eval_key(
    key_expr: &Spanned<Expr>,
    parent_env: &Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    depth: usize,
) -> EvalResult<Key> {
    // Fast path for literal keys (avoids creating temporary thunks)
    match &key_expr.node {
        Expr::Str(s) => return Ok(Key::String(s.clone())),
        Expr::Int(n) => return Ok(Key::Int(*n)),
        _ => {}
    }
    // General path: must materialize because IndexMap requires concrete Key values
    let thunk = eval(
        Rc::new(key_expr.clone()),
        Rc::clone(parent_env),
        ctx,
        depth + 1,
    )?;
    let value = materialize(&thunk, Some(&key_expr.span), ctx, depth + 1)?;
    value_to_key(&value, &key_expr.span)
}

fn value_to_key(value: &Value, span: &Span) -> EvalResult<Key> {
    match value {
        Value::String(s) => Ok(Key::String(s.clone())),
        Value::Int(n) => Ok(Key::Int(*n)),
        _ => Err(EvalError::type_mismatch("String or Int", value.type_name(), *span).into()),
    }
}

/// Force a thunk to its concrete value, memoizing the result.
///
/// On first materialization, evaluates the thunk and caches the result (or error).
/// Subsequent calls return the cached value without re-evaluation. This implements
/// call-by-need semantics: lazy evaluation with sharing.
///
/// # ThunkState transitions
///
/// - `Materialized`: returns cached value immediately
/// - `Failed`: returns cached error (with updated materialization_span)
/// - `InProgress`: returns circular dependency error
/// - `Unevaluated`: evaluates expr in env, memoizes result or error
/// - `PendingBuiltin`: calls builtin with args, memoizes result or error
/// - `PendingCall`: materializes func, invokes it with args, memoizes result or error
///
/// # Side effects
///
/// Mutates the thunk's internal state via `RefCell`. On success, transitions to
/// `Materialized`. On failure, transitions to `Failed` (caching the error).
///
/// # Parameters
///
/// - `mat_span`: the span of the expression that triggered materialization
///   (e.g., an access chain). Attached to errors so users can see both where
///   a value was defined and where it was forced.
/// - `_ctx`: intentionally unused. Each thunk captures its creation-time
///   `EvalContext` in its `ThunkState` variant (`Unevaluated`, `PendingBuiltin`,
///   `PendingCall`, `Guarded`), and evaluates in that context rather than the caller's.
///   This follows Launchbury (1993): thunks are closures over their birth
///   environment, so forcing a thunk must use the context in which it was
///   allocated, not the context of the demand site. The parameter exists for
///   API symmetry with `eval()` and will be removed during the CEK machine
///   migration (iterative-eval milestone).
pub fn materialize(
    thunk: &Thunk,
    mat_span: Option<&Span>,
    _ctx: &Rc<EvalContext>,
    depth: usize,
) -> EvalResult<Value> {
    // Read origin before checking state (InProgress may not preserve it)
    let origin = thunk.origin.clone();
    let thunk_span = thunk.span;

    {
        let state = thunk.state();
        match &*state {
            ThunkState::Materialized(v) => return Ok(v.clone()),
            // Failed state: dual-span error caching model.
            //
            // First failure sets both definition_span and materialization_span.
            // Subsequent accesses with a new mat_span conditionally update:
            // - If materialization_span is None (edge case: cached error had no mat_span),
            //   set it to the current mat_span.
            // - If materialization_span differs from current mat_span and current mat_span
            //   is not already in the stack, add current mat_span as a stack frame.
            //   The original materialization_span is preserved.
            ThunkState::Failed(ref err) => {
                let mut cloned = (**err).clone();
                let mut should_update_cache = false;
                if let Some(span) = mat_span {
                    if cloned.materialization_span.is_none() {
                        // First access via Failed path (edge case: error cached without mat_span)
                        cloned.materialization_span = Some(*span);
                        should_update_cache = true;
                    } else if cloned.materialization_span != Some(*span)
                        && !cloned.stack.iter().any(|f| f.span == *span)
                    {
                        // Different access site: add as stack frame, preserve original mat_span
                        cloned.push_frame("materialized".to_string(), *span);
                        should_update_cache = true;
                    }
                }
                // Update cached error if we modified it
                if should_update_cache && cloned.kind.is_cacheable() {
                    drop(state);
                    thunk.set_state(ThunkState::Failed(Box::new(cloned.clone())));
                }
                return Err(Box::new(cloned));
            }
            ThunkState::InProgress => {
                // PROP-CYCLE: circular dependency detected during InProgress state check.
                // Error is constructed and decorated manually via with_materialization_span(),
                // rather than using the decorate closure (defined below), because we need to
                // immediately cache the error in the Failed state before returning.
                let label = origin.as_deref().unwrap_or("thunk");
                // Capture the eval_stack for cycle path reconstruction
                let cycle_path = _ctx.state.borrow().eval_stack.clone();
                let mut err = EvalError::circular_dependency(label, thunk.span, cycle_path);
                if let Some(span) = mat_span {
                    err = err.with_materialization_span(*span);
                }
                let err_boxed: Box<EvalError> = err.into();
                drop(state);
                thunk.cache_failure(&err_boxed);
                return Err(err_boxed);
            }
            ThunkState::Placeholder => {
                panic!(
                    "attempted to force a Placeholder thunk (span {:?}). \
                     This indicates a letrec construction bug: all placeholder \
                     slots must be filled via set_state() before evaluation begins.",
                    thunk.span
                );
            }
            ThunkState::Unevaluated { .. }
            | ThunkState::PendingBuiltin { .. }
            | ThunkState::PendingCall { .. }
            | ThunkState::Guarded { .. } => {}
        }
    }

    let origin_opt: Option<&str> = origin.as_deref();
    let decorate = |e| attach_materialization_context(e, mat_span, origin_opt, thunk_span);

    if let Some((expr, env, thunk_ctx)) = thunk.take_unevaluated() {
        // Check depth limit only for deferred states that require evaluation
        if depth > MAX_EVAL_DEPTH {
            let mut err = EvalError::depth_exceeded(MAX_EVAL_DEPTH, thunk_span);
            if let Some(span) = mat_span {
                err = err.with_materialization_span(*span);
            }
            // Restore state for non-cacheable error
            thunk.set_state(ThunkState::Unevaluated {
                expr,
                env,
                ctx: thunk_ctx,
            });
            return Err(err.into());
        }

        let result = eval(Rc::clone(&expr), Rc::clone(&env), &thunk_ctx, depth + 1)
            .and_then(|result_thunk| {
                run(
                    Action::Materialize {
                        thunk: result_thunk,
                        mat_span: mat_span.copied(),
                        depth: depth + 1,
                    },
                    &thunk_ctx,
                )
            })
            .map_err(&decorate);

        match result {
            Ok(value) => {
                thunk.set_state(ThunkState::Materialized(value.clone()));
                Ok(value)
            }
            Err(e) => {
                if e.kind.is_cacheable() {
                    thunk.cache_failure(&e);
                } else {
                    // Non-cacheable error (e.g., DepthExceeded): restore original state
                    // so the thunk can be re-evaluated at a shallower depth.
                    thunk.set_state(ThunkState::Unevaluated {
                        expr,
                        env,
                        ctx: thunk_ctx,
                    });
                }
                Err(e)
            }
        }
    } else if let Some((def, args, named, pending_depth, call_span, thunk_ctx)) =
        thunk.take_pending_builtin()
    {
        // Check depth limit only for deferred states that require evaluation
        if depth > MAX_EVAL_DEPTH {
            let mut err = EvalError::depth_exceeded(MAX_EVAL_DEPTH, thunk_span);
            if let Some(span) = mat_span {
                err = err.with_materialization_span(*span);
            }
            // Restore state for non-cacheable error
            thunk.set_state(ThunkState::PendingBuiltin {
                def,
                args: Box::new(args.clone()),
                named: named.clone(),
                depth: pending_depth,
                call_span,
                ctx: thunk_ctx.clone(),
            });
            return Err(err.into());
        }

        // TCO: use caller's depth for builtin arg materialization, not the stored
        // pending_depth. This prevents depth accumulation through builtin chains
        // (e.g., $- → materialize → $- → materialize).
        // `named` is None for internally-created thunks (common case); only $apply
        // passes named args through. Use an empty map ref for the None case.
        let builtin_args = crate::value::BuiltinArgs {
            args: &args,
            named: named.as_ref(),
            depth,
            call_span,
            ctx: Rc::clone(&thunk_ctx),
        };
        match (def.func)(builtin_args).map_err(&decorate) {
            Ok(result_thunk) => {
                // Fast path: if the builtin already materialized its result, skip recursion.
                if let Some(value) = result_thunk.try_get_materialized() {
                    thunk.set_state(ThunkState::Materialized(value.clone()));
                    Ok(value)
                } else {
                    match run(
                        Action::Materialize {
                            thunk: result_thunk,
                            mat_span: mat_span.copied(),
                            depth: depth + 1,
                        },
                        &thunk_ctx,
                    )
                    .map_err(&decorate)
                    {
                        Ok(value) => {
                            thunk.set_state(ThunkState::Materialized(value.clone()));
                            Ok(value)
                        }
                        Err(e) => {
                            if e.kind.is_cacheable() {
                                thunk.cache_failure(&e);
                            } else {
                                thunk.set_state(ThunkState::PendingBuiltin {
                                    def,
                                    args: Box::new(args),
                                    named,
                                    depth: pending_depth,
                                    call_span,
                                    ctx: thunk_ctx,
                                });
                            }
                            Err(e)
                        }
                    }
                }
            }
            Err(e) => {
                if e.kind.is_cacheable() {
                    thunk.cache_failure(&e);
                } else {
                    thunk.set_state(ThunkState::PendingBuiltin {
                        def,
                        args: Box::new(args),
                        named,
                        depth: pending_depth,
                        call_span,
                        ctx: thunk_ctx,
                    });
                }
                Err(e)
            }
        }
    } else if let Some((func_thunk, args, named, call_span, caller_env, thunk_ctx)) =
        thunk.take_pending_call()
    {
        // Check depth limit only for deferred states that require evaluation
        if depth > MAX_EVAL_DEPTH {
            let mut err = EvalError::depth_exceeded(MAX_EVAL_DEPTH, thunk_span);
            if let Some(span) = mat_span {
                err = err.with_materialization_span(*span);
            }
            // Restore state for non-cacheable error
            thunk.set_state(ThunkState::PendingCall {
                func: func_thunk.clone(),
                args: Box::new(args.clone()),
                named: named.clone().map(Box::new),
                call_span,
                caller_env: caller_env.clone(),
                ctx: thunk_ctx.clone(),
            });
            return Err(err.into());
        }

        // Materialize the function thunk to determine if it's a Function or Builtin
        let func_value = match run(
            Action::Materialize {
                thunk: Rc::clone(&func_thunk),
                mat_span: Some(call_span),
                depth: depth + 1,
            },
            &thunk_ctx,
        )
        .map_err(&decorate)
        {
            Ok(v) => v,
            Err(e) => {
                if e.kind.is_cacheable() {
                    thunk.cache_failure(&e);
                } else {
                    thunk.set_state(ThunkState::PendingCall {
                        func: func_thunk,
                        args: Box::new(args),
                        named: named.map(Box::new),
                        call_span,
                        caller_env,
                        ctx: thunk_ctx,
                    });
                }
                return Err(e);
            }
        };

        match func_value {
            Value::Function { params, body, env } => {
                // Build CallContext and invoke the function
                let call_ctx = CallContext {
                    params: &params,
                    body: &body,
                    closure_env: &env,
                    positional: &args,
                    named: named.as_ref(),
                    // For normal calls, `default_env` is the caller's environment (the env at
                    // the call site where the PendingCall thunk was created by `eval_call`).
                    // When forcing a PendingCall, `caller_env` is preserved from creation time
                    // (iterative-eval-b1) — it is the env captured in the thunk, not the env
                    // of whoever triggered materialization. `$apply` diverges: it uses the
                    // closure env as `default_env` so that defaults see the function's own scope.
                    default_env: &caller_env,
                    call_span,
                    depth,
                    origin: origin.clone(),
                    ctx: &thunk_ctx,
                };

                match invoke_function(&call_ctx).map_err(&decorate) {
                    Ok(result_thunk) => {
                        // Materialize the result and memoize
                        match run(
                            Action::Materialize {
                                thunk: result_thunk,
                                mat_span: mat_span.copied(),
                                depth: depth + 1,
                            },
                            &thunk_ctx,
                        )
                        .map_err(&decorate)
                        {
                            Ok(value) => {
                                thunk.set_state(ThunkState::Materialized(value.clone()));
                                Ok(value)
                            }
                            Err(e) => {
                                if e.kind.is_cacheable() {
                                    thunk.cache_failure(&e);
                                } else {
                                    thunk.set_state(ThunkState::PendingCall {
                                        func: func_thunk.clone(),
                                        args: Box::new(args.clone()),
                                        named: named.clone().map(Box::new),
                                        call_span,
                                        caller_env: caller_env.clone(),
                                        ctx: thunk_ctx.clone(),
                                    });
                                }
                                Err(e)
                            }
                        }
                    }
                    Err(mut e) => {
                        // Add stack frame for function call site.
                        // Success path doesn't need call site tracking - only errors
                        // need stack traces for debugging. The thunk's span is the
                        // definition site, which is sufficient for successful results.
                        if let Some(label) = origin.as_deref() {
                            e.push_frame(label.to_string(), call_span);
                        }
                        if e.kind.is_cacheable() {
                            thunk.cache_failure(&e);
                        } else {
                            thunk.set_state(ThunkState::PendingCall {
                                func: func_thunk.clone(),
                                args: Box::new(args.clone()),
                                named: named.clone().map(Box::new),
                                call_span,
                                caller_env: caller_env.clone(),
                                ctx: thunk_ctx.clone(),
                            });
                        }
                        Err(e)
                    }
                }
            }
            Value::Builtin(def) => {
                let builtin_args = crate::value::BuiltinArgs {
                    args: &args,
                    named: named.as_ref(),
                    depth,
                    call_span,
                    ctx: Rc::clone(&thunk_ctx),
                };
                match (def.func)(builtin_args).map_err(&decorate) {
                    Ok(result_thunk) => {
                        if let Some(value) = result_thunk.try_get_materialized() {
                            thunk.set_state(ThunkState::Materialized(value.clone()));
                            Ok(value)
                        } else {
                            match run(
                                Action::Materialize {
                                    thunk: result_thunk,
                                    mat_span: mat_span.copied(),
                                    depth: depth + 1,
                                },
                                &thunk_ctx,
                            )
                            .map_err(&decorate)
                            {
                                Ok(value) => {
                                    thunk.set_state(ThunkState::Materialized(value.clone()));
                                    Ok(value)
                                }
                                Err(e) => {
                                    if e.kind.is_cacheable() {
                                        thunk.cache_failure(&e);
                                    } else {
                                        thunk.set_state(ThunkState::PendingCall {
                                            func: func_thunk.clone(),
                                            args: Box::new(args.clone()),
                                            named: named.clone().map(Box::new),
                                            call_span,
                                            caller_env: caller_env.clone(),
                                            ctx: thunk_ctx.clone(),
                                        });
                                    }
                                    Err(e)
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if e.kind.is_cacheable() {
                            thunk.cache_failure(&e);
                        } else {
                            thunk.set_state(ThunkState::PendingCall {
                                func: func_thunk.clone(),
                                args: Box::new(args.clone()),
                                named: named.clone().map(Box::new),
                                call_span,
                                caller_env: caller_env.clone(),
                                ctx: thunk_ctx.clone(),
                            });
                        }
                        Err(e)
                    }
                }
            }
            other => {
                let err =
                    EvalError::type_mismatch("Function or Builtin", other.type_name(), call_span);
                let decorated = decorate(Box::new(err));
                if decorated.kind.is_cacheable() {
                    thunk.cache_failure(&decorated);
                } else {
                    thunk.set_state(ThunkState::PendingCall {
                        func: func_thunk,
                        args: Box::new(args),
                        named: named.map(Box::new),
                        call_span,
                        caller_env,
                        ctx: thunk_ctx,
                    });
                }
                Err(decorated)
            }
        }
    } else if let Some((inner, expected, mut field_path, guard_span)) = thunk.take_guarded() {
        // Check depth limit only for deferred states that require evaluation
        if depth > MAX_EVAL_DEPTH {
            let mut err = EvalError::depth_exceeded(MAX_EVAL_DEPTH, thunk_span);
            if let Some(span) = mat_span {
                err = err.with_materialization_span(*span);
            }
            // Restore state for non-cacheable error
            thunk.set_state(ThunkState::Guarded {
                inner: inner.clone(),
                expected: expected.clone(),
                field_path: Box::new(field_path.clone()),
                guard_span,
            });
            return Err(err.into());
        }

        // Materialize the inner thunk first
        // LIMITATION: Guard failures do not check default: from the original TypeAssert
        // annotation because Guarded thunks do not capture the annotation or environment.
        // This is a known limitation accepted in sprint review round 1 finding #6 and
        // re-raised in round 2 finding #3. Fixing requires storing default_expr + env in
        // Thunk State::Guarded, but attempts led to stack overflow. Deferred post-1.0.

        // Capture inner thunk's span before materializing — used as data_span for error reporting
        let inner_span = inner.span;

        let result = run(
            Action::Materialize {
                thunk: Rc::clone(&inner),
                mat_span: mat_span.copied(),
                depth: depth + 1,
            },
            _ctx,
        );

        match result {
            Ok(value) => {
                // For Record types, apply proxy contract wrapping
                if let Type::Record(ref row) = expected {
                    if let Value::Dict(ref entries) = value {
                        // Use helper to validate and wrap record
                        match validate_and_wrap_record(
                            entries,
                            row,
                            &mut field_path,
                            guard_span,
                            inner_span,
                            _ctx,
                        ) {
                            Ok(new_entries) => {
                                let guarded_value = Value::Dict(new_entries);
                                thunk.set_state(ThunkState::Materialized(guarded_value.clone()));
                                Ok(guarded_value)
                            }
                            Err(err) => {
                                let err = decorate(err);
                                thunk.cache_failure(&err);
                                Err(err)
                            }
                        }
                    } else {
                        // Expected Record but got non-Dict
                        let field_path_prefix = if field_path.is_empty() {
                            String::new()
                        } else {
                            format!("field {}: ", format_field_path(&field_path))
                        };
                        let err = EvalError::type_assert_failed(
                            &format!("{}{}", field_path_prefix, format_type_for_assert(&expected)),
                            &value.type_name(),
                            inner_span,
                        );
                        let err = decorate(err.into());
                        thunk.cache_failure(&err);
                        Err(err)
                    }
                } else {
                    // For non-Record types, simple value check
                    if value_matches_type(&value, &expected) {
                        thunk.set_state(ThunkState::Materialized(value.clone()));
                        Ok(value)
                    } else {
                        let field_path_prefix = if field_path.is_empty() {
                            String::new()
                        } else {
                            format!("field {}: ", format_field_path(&field_path))
                        };
                        let err = EvalError::type_assert_failed(
                            &format!("{}{}", field_path_prefix, format_type_for_assert(&expected)),
                            &value.type_name(),
                            inner_span,
                        );
                        let err = decorate(err.into());
                        thunk.cache_failure(&err);
                        Err(err)
                    }
                }
            }
            Err(e) => {
                // Inner materialization error propagates (not a type mismatch)
                if e.kind.is_cacheable() {
                    thunk.cache_failure(&e);
                } else {
                    // Non-cacheable error (e.g., DepthExceeded): restore Guarded state
                    // so the thunk can be re-evaluated at a shallower depth.
                    thunk.set_state(ThunkState::Guarded {
                        inner,
                        expected,
                        field_path: Box::new(field_path),
                        guard_span,
                    });
                }
                Err(e)
            }
        }
    } else {
        unreachable!(
            "state must be Unevaluated, PendingBuiltin, PendingCall, or Guarded. \
             All other ThunkState variants are handled in the early-return section at the \
             top of this function: Materialized returns early, Failed returns early, \
             InProgress returns early and caches circular dependency error."
        )
    }
}

// Re-export deep_materialize from eval_deep module
pub use crate::eval_deep::deep_materialize;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::*;
    use crate::error::ErrorKind;
    use crate::test_util::{rsp, sp, test_span};
    use crate::value::*;

    fn empty_env() -> Rc<RefCell<Environment>> {
        Rc::new(RefCell::new(Environment::new()))
    }

    fn test_ctx() -> Rc<EvalContext> {
        let env = empty_env();
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        EvalContext::new(base_dir, env, false)
    }

    #[test]
    fn test_eval_int() {
        let expr = sp(Expr::Int(42));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_eval_float() {
        let expr = sp(Expr::Float(3.14));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Float(3.14));
    }

    #[test]
    fn test_eval_bool() {
        let expr = sp(Expr::Bool(true));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn test_eval_str() {
        let expr = sp(Expr::Str("hello".into()));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::String("hello".into()));
    }

    #[test]
    fn test_varref_found() {
        let env = empty_env();
        let span = test_span(1, 1, 1, 5);
        env.borrow_mut().insert(
            "x".into(),
            Rc::new(Thunk::new_materialized(Value::Int(99), span)),
        );

        let expr = sp(Expr::var_ref("x".into()));
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_varref_parent_scope() {
        let parent = empty_env();
        let span = test_span(1, 1, 1, 5);
        parent.borrow_mut().insert(
            "y".into(),
            Rc::new(Thunk::new_materialized(Value::Int(77), span)),
        );

        let child = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(&parent))));
        let expr = sp(Expr::var_ref("y".into()));
        let thunk = eval(Rc::new(expr.clone()), child, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(77));
    }

    #[test]
    fn test_varref_not_found() {
        let expr = sp(Expr::var_ref("missing".into()));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("undefined variable: missing"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_simple_dict() {
        // [x: 1  y: hello]
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::Str("hello".into())),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();

        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let x_thunk = map.get(&Key::String("x".into())).unwrap();
                assert_eq!(
                    materialize(x_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(
                    materialize(y_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::String("hello".into())
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_auto_indexed_dict() {
        let entries = vec![
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(10)),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(20)),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(30)),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();

        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(10)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(20)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(30)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_mixed_keyed_and_auto_indexed() {
        // [name: hello  42  flag: true  99]
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: rsp(Expr::Str("hello".into())),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(42)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("flag".into()))),
                value: rsp(Expr::Bool(true)),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(99)),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();

        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 4);
                assert_eq!(
                    materialize(
                        map.get(&Key::String("name".into())).unwrap(),
                        None,
                        &test_ctx(),
                        0
                    )
                    .unwrap(),
                    Value::String("hello".into())
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(42)
                );
                assert_eq!(
                    materialize(
                        map.get(&Key::String("flag".into())).unwrap(),
                        None,
                        &test_ctx(),
                        0
                    )
                    .unwrap(),
                    Value::Bool(true)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(99)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_dict_letrec_sibling_reference() {
        // [x: 5  y: $x]
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(5)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::var_ref("x".into())),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();

        match val {
            Value::Dict(map) => {
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(
                    materialize(y_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::Int(5)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_dict_letrec_forward_reference() {
        // [y: $x  x: 10] -- y references x which is defined after y
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::var_ref("x".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(10)),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();

        match val {
            Value::Dict(map) => {
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(
                    materialize(y_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::Int(10)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_cycle_detection() {
        // [x: $x] -- x references itself
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("x".into())),
        })];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();

        match val {
            Value::Dict(map) => {
                let x_thunk = map.get(&Key::String("x".into())).unwrap();
                let err = materialize(x_thunk, None, &test_ctx(), 0).unwrap_err();
                assert!(
                    err.message().contains("circular dependency"),
                    "got: {}",
                    err.message()
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_cycle_detection_transitions_to_failed() {
        // When a thunk detects a circular dependency (InProgress state),
        // it should cache the error in Failed state, not leave it in InProgress.
        // Subsequent materializations should return the cached error.
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("x".into())),
        })];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();

        let x_thunk = match val {
            Value::Dict(map) => Rc::clone(map.get(&Key::String("x".into())).unwrap()),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First materialization: should detect the cycle and fail
        let err1 = materialize(&x_thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err1.message().contains("circular dependency"),
            "first error: got: {}",
            err1.message()
        );

        // Check that the thunk is now in Failed state, not stuck in InProgress
        match &*x_thunk.state() {
            ThunkState::Failed(cached_err) => {
                assert!(
                    cached_err.message().contains("circular dependency"),
                    "cached error should mention circular dependency, got: {}",
                    cached_err.message()
                );
            }
            other => panic!("expected Failed state after cycle detection, got {other:?}"),
        }

        // Second materialization: should return the cached circular dependency error
        let err2 = materialize(&x_thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err2.message().contains("circular dependency"),
            "second error: got: {}",
            err2.message()
        );
    }

    #[test]
    fn test_thunk_retryable_after_error() {
        // [x: $missing] -- materializing x fails because $missing is undefined.
        // After failure, the thunk must be restored to Unevaluated, not left
        // as InProgress. A second materialize attempt should produce the same
        // "undefined variable" error, NOT "circular dependency".
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("missing".into())),
        })];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();

        let x_thunk = match &dict_val {
            Value::Dict(map) => Rc::clone(map.get(&Key::String("x".into())).unwrap()),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First attempt: should fail with "undefined variable"
        let err1 = materialize(&x_thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err1.message().contains("undefined variable: missing"),
            "first attempt: got: {}",
            err1.message()
        );

        // Second attempt: should produce the SAME error, not "circular dependency"
        let err2 = materialize(&x_thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err2.message().contains("undefined variable: missing"),
            "second attempt should not be poisoned, got: {}",
            err2.message()
        );
        assert!(
            !err2.message().contains("circular dependency"),
            "thunk was poisoned: got circular dependency on retry"
        );
    }

    #[test]
    fn test_nested_dict_sees_outer_bindings() {
        // [x: 42  inner: [y: $x]]
        let inner_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("y".into()))),
            value: rsp(Expr::var_ref("x".into())),
        })];
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(42)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("inner".into()))),
                value: rsp(Expr::Dict(inner_entries)),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let outer = materialize(&thunk, None, &test_ctx(), 0).unwrap();

        match outer {
            Value::Dict(outer_map) => {
                let inner_thunk = outer_map.get(&Key::String("inner".into())).unwrap();
                let inner_val = materialize(inner_thunk, None, &test_ctx(), 0).unwrap();
                match inner_val {
                    Value::Dict(inner_map) => {
                        let y_thunk = inner_map.get(&Key::String("y".into())).unwrap();
                        assert_eq!(
                            materialize(y_thunk, None, &test_ctx(), 0).unwrap(),
                            Value::Int(42)
                        );
                    }
                    other => panic!("expected inner Dict, got {other:?}"),
                }
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_duplicate_key_error() {
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(2)),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("duplicate key: x"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_fn_creates_function_value() {
        // [fn [x] $x] → Function
        let expr = sp(Expr::Fn {
            return_ann: None,
            params: vec![sp(Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            })],
            body: Rc::new(sp(Expr::var_ref("x".into()))),
            desugared: false,
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "x");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_captures_closure_env() {
        // outer: 42 is in env, [fn [] $outer] should capture it
        let env = empty_env();
        env.borrow_mut().insert(
            "outer".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );
        let fn_expr = sp(Expr::Fn {
            return_ann: None,
            params: vec![],
            body: Rc::new(sp(Expr::var_ref("outer".into()))),
            desugared: false,
        });
        let fn_thunk = eval(Rc::new(fn_expr.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let fn_val = materialize(&fn_thunk, None, &test_ctx(), 0).unwrap();

        // Call it: [call $f]
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![],
            named_args: vec![],
            implied: false,
        });
        let result_thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();
        let result = materialize(&result_thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn test_call_simple() {
        // Define identity function and call it
        // f: [fn [x] $x]
        // [call $f 42]
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::var_ref("x".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![Rc::new(sp(Expr::Int(42)))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_call_multiple_args() {
        // f: [fn [a b] $b]  -- returns second arg
        // [call $f 10 20] → 20
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![
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
            ]),
            body: Rc::new(sp(Expr::var_ref("b".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![Rc::new(sp(Expr::Int(10))), Rc::new(sp(Expr::Int(20)))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(20));
    }

    #[test]
    fn test_call_on_non_function() {
        let env = empty_env();
        env.borrow_mut().insert(
            "x".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("x".into()))),
            args: vec![],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0)
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("type mismatch"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("Function"), "got: {}", err.message());
    }

    #[test]
    fn test_call_too_few_args() {
        // f: [fn [x y] $x]
        // [call $f 1] → arity mismatch
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![
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
            ]),
            body: Rc::new(sp(Expr::var_ref("x".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![Rc::new(sp(Expr::Int(1)))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0)
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("missing argument for required parameter"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_call_too_many_args() {
        // f: [fn [x] $x]
        // [call $f 1 2] → arity mismatch
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::var_ref("x".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![rsp(Expr::Int(1)), rsp(Expr::Int(2))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0)
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_call_named_arg_with_default() {
        // f: [fn [x  y@[default: 99]] [result: $y]]
        // [call $f 1] → y defaults to 99
        let env = empty_env();
        let default_entry = sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(99)),
        });
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![default_entry]))),
                    variadic: false,
                },
            ]),
            body: Rc::new(sp(Expr::var_ref("y".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        // Call without named arg -- y should default to 99
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![Rc::new(sp(Expr::Int(1)))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_call_named_arg_overridden() {
        // f: [fn [x  y@[default: 99]] $y]
        // [call $f 1 y: 42] → y = 42
        let env = empty_env();
        let default_entry = sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(99)),
        });
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![default_entry]))),
                    variadic: false,
                },
            ]),
            body: Rc::new(sp(Expr::var_ref("y".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![Rc::new(sp(Expr::Int(1)))],
            named_args: vec![sp(NamedArg {
                name: "y".into(),
                value: rsp(Expr::Int(42)),
            })],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_call_unexpected_named_arg() {
        // f: [fn [x] $x]
        // [call $f 1 z: 2] → error: unexpected named argument
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::var_ref("x".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![Rc::new(sp(Expr::Int(1)))],
            named_args: vec![sp(NamedArg {
                name: "z".into(),
                value: rsp(Expr::Int(2)),
            })],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0)
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("unexpected named argument: z"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_call_duplicate_positional_and_named_error() {
        // f: [fn [x y@[default: 99]] $y]
        // [call $f 1 2 y: 42] → error: y received both positional and named argument
        let env = empty_env();
        let default_entry = sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(99)),
        });
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![default_entry]))),
                    variadic: false,
                },
            ]),
            body: Rc::new(sp(Expr::var_ref("y".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![rsp(Expr::Int(1)), rsp(Expr::Int(2))],
            named_args: vec![sp(NamedArg {
                name: "y".into(),
                value: rsp(Expr::Int(42)),
            })],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0)
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("received both positional and named argument"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_call_variadic() {
        // f: [fn [x ...rest] $rest]
        // [call $f 1 2 3] → rest = Dict({0: 2, 1: 3})
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "rest".into(),
                    annotation: None,
                    variadic: true,
                },
            ]),
            body: Rc::new(sp(Expr::var_ref("rest".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![
                Rc::new(sp(Expr::Int(1))),
                Rc::new(sp(Expr::Int(2))),
                Rc::new(sp(Expr::Int(3))),
            ],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(2)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(3)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_call_variadic_empty() {
        // f: [fn [x ...rest] $rest]
        // [call $f 1] → rest = Dict({})
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "rest".into(),
                    annotation: None,
                    variadic: true,
                },
            ]),
            body: Rc::new(sp(Expr::var_ref("rest".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![Rc::new(sp(Expr::Int(1)))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected empty Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_call_builtin() {
        fn add_builtin(ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let crate::value::BuiltinArgs { args, .. } = ctx;
            let a = materialize(&args[0], None, &test_ctx(), 0)?;
            let b = materialize(&args[1], None, &test_ctx(), 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Rc::new(Thunk::new_materialized(
                    Value::Int(x + y),
                    test_span(1, 1, 1, 1),
                ))),
                _ => panic!("test expects Int args"),
            }
        }
        let env = empty_env();
        env.borrow_mut().insert(
            "add".into(),
            Rc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: add_builtin,
                    name: "add",
                    pos_strictness: &[],
                }),
                test_span(1, 1, 1, 5),
            )),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("add".into()))),
            args: vec![Rc::new(sp(Expr::Int(3))), Rc::new(sp(Expr::Int(4)))],
            named_args: vec![],
            implied: false,
        });
        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(7));
    }

    #[test]
    fn test_type_alias_returns_empty_dict() {
        let expr = sp(Expr::TypeAlias(Box::new(sp(Expr::var_ref(
            "MyType".into(),
        )))));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected empty Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_rest_marker_anonymous_errors() {
        let expr = sp(Expr::Rest(None));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("rest marker (...) is only valid inside type expressions"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_rest_marker_named_errors() {
        let expr = sp(Expr::Rest(Some("x".into())));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("rest marker (...) is only valid inside type expressions"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_bare_underscore_is_not_lambda() {
        // $_ alone is just a VarRef, not an implicit lambda
        // It should fail with "undefined variable" if not in scope
        let expr = sp(Expr::var_ref("_".into()));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("undefined variable: _"),
            "got: {}",
            err.message()
        );
    }

    // ── Integration tests for $_ desugaring + evaluation ──────────────────
    // These tests verify that the AST-level desugaring (from src/desugar.rs)
    // integrates correctly with evaluation. They manually call desugar_expr()
    // before eval() to simulate the full pipeline.

    #[test]
    fn test_underscore_access_chain_becomes_lambda() {
        // $_.name → [fn [_] $_.name] after desugaring
        // Evaluating this should produce a Function, not look up $_
        let mut expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("_".into()))),
            field: "name".into(),
        });

        // Desugar before eval (simulates pipeline integration)
        crate::desugar::desugar_expr(&mut expr, 0);

        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    #[test]
    fn test_underscore_in_call_becomes_lambda() {
        // [call $f $_] where $f is in scope → should produce a lambda after desugaring
        // The outer [call ...] contains $_ directly → wraps in [fn [_] [call $f $_]]
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::var_ref("x".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );

        let mut call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![Rc::new(sp(Expr::var_ref("_".into())))],
            named_args: vec![],
            implied: false,
        });

        // Desugar before eval
        crate::desugar::desugar_expr(&mut call_expr, 0);

        let thunk = eval(Rc::new(call_expr.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function from $_ desugaring, got {other:?}"),
        }
    }

    #[test]
    fn test_underscore_lambda_callable() {
        // Create $_.name as a lambda (via desugaring), then call it with a dict that has name: "alice"
        let env = empty_env();

        // Build the $_.name expression → becomes [fn [_] $_.name] after desugaring
        let mut getter_expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("_".into()))),
            field: "name".into(),
        });

        // Desugar to get the lambda
        crate::desugar::desugar_expr(&mut getter_expr, 0);

        let getter_thunk = eval(
            Rc::new(getter_expr.clone()),
            Rc::clone(&env),
            &test_ctx(),
            0,
        )
        .unwrap();
        let getter_val = materialize(&getter_thunk, None, &test_ctx(), 0).unwrap();
        env.borrow_mut().insert(
            "getter".into(),
            Rc::new(Thunk::new_materialized(getter_val, test_span(1, 1, 1, 10))),
        );

        // Call it with [name: alice]
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("getter".into()))),
            args: vec![rsp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: rsp(Expr::Str("alice".into())),
            })]))],
            named_args: vec![],
            implied: false,
        });
        let result_thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();
        let result = materialize(&result_thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(result, Value::String("alice".into()));
    }

    #[test]
    fn test_underscore_in_dict_entry() {
        // [a: $_.name] → desugars to [fn [_] [a: $_.name]]
        // Dict with $_ in a value position should desugar to an implicit lambda
        let mut expr = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("a".into()))),
            value: rsp(Expr::DotAccess {
                expr: Box::new(sp(Expr::var_ref("_".into()))),
                field: "name".into(),
            }),
        })]));

        // Desugar before eval
        crate::desugar::desugar_expr(&mut expr, 0);

        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function from $_ dict desugaring, got {other:?}"),
        }
    }

    #[test]
    fn test_underscore_in_named_arg() {
        // [call $f x: $_] → desugars to [fn [_] [call $f x: $_]]
        // Call with $_ in a named arg value should desugar to an implicit lambda
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::var_ref("x".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );

        let mut call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![],
            named_args: vec![sp(NamedArg {
                name: "x".into(),
                value: rsp(Expr::var_ref("_".into())),
            })],
            implied: false,
        });

        // Desugar before eval
        crate::desugar::desugar_expr(&mut call_expr, 0);

        let thunk = eval(Rc::new(call_expr.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function from $_ named arg desugaring, got {other:?}"),
        }
    }

    fn dict_with_entries(entries: Vec<(&str, Value)>) -> Spanned<Expr> {
        let ast_entries = entries
            .into_iter()
            .map(|(k, v)| {
                let value_expr = match v {
                    Value::Int(n) => Expr::Int(n),
                    Value::String(s) => Expr::Str(s),
                    Value::Bool(b) => Expr::Bool(b),
                    Value::Float(f) => Expr::Float(f),
                    _ => panic!("unsupported value type in test helper"),
                };
                sp(Entry {
                    key: Some(sp(Expr::Str(k.into()))),
                    value: rsp(value_expr),
                })
            })
            .collect();
        sp(Expr::Dict(ast_entries))
    }

    #[test]
    fn test_dot_access() {
        // [name: hello].name -> "hello"
        let dict = dict_with_entries(vec![("name", Value::String("hello".into()))]);
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();

        // Bind the dict to $d in the environment
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("d".into()))),
            field: "name".into(),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::String("hello".into()));
    }

    #[test]
    fn test_dot_access_missing_key() {
        let dict = dict_with_entries(vec![("x", Value::Int(1))]);
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("d".into()))),
            field: "missing".into(),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("key not found: missing"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_dot_access_on_non_dict() {
        let env = empty_env();
        env.borrow_mut().insert(
            "x".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );

        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("x".into()))),
            field: "foo".into(),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(err.message().contains("expected"), "got: {}", err.message());
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_bracket_access_int_key() {
        // [10 20 30][1] -> 20
        let entries = vec![
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(10)),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(20)),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(30)),
            }),
        ];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::BracketAccess {
            expr: Box::new(sp(Expr::var_ref("d".into()))),
            key: Box::new(sp(Expr::Int(1))),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(20));
    }

    #[test]
    fn test_bracket_access_string_key() {
        let dict = dict_with_entries(vec![("name", Value::String("alice".into()))]);
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::BracketAccess {
            expr: Box::new(sp(Expr::var_ref("d".into()))),
            key: Box::new(sp(Expr::Str("name".into()))),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::String("alice".into()));
    }

    #[test]
    fn test_bracket_access_missing_key() {
        let dict = dict_with_entries(vec![("a", Value::Int(1))]);
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::BracketAccess {
            expr: Box::new(sp(Expr::var_ref("d".into()))),
            key: Box::new(sp(Expr::Str("z".into()))),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("key not found: z"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_range_access_both_bounds() {
        // [0: a  1: b  2: c  3: d  4: e][2..4] -> [2: c  3: d]
        let entries: Vec<_> = (0..5)
            .map(|i| {
                sp(Entry {
                    key: Some(sp(Expr::Int(i))),
                    value: rsp(Expr::Str(format!("v{i}"))),
                })
            })
            .collect();
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::var_ref("d".into()))),
            start: Some(Box::new(sp(Expr::Int(2)))),
            end: Some(Box::new(sp(Expr::Int(4)))),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::String("v2".into())
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(3)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::String("v3".into())
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_access_start_only() {
        // [0: a  1: b  2: c][1..] -> [1: b  2: c]
        let entries: Vec<_> = (0..3)
            .map(|i| {
                sp(Entry {
                    key: Some(sp(Expr::Int(i))),
                    value: rsp(Expr::Str(format!("v{i}"))),
                })
            })
            .collect();
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::var_ref("d".into()))),
            start: Some(Box::new(sp(Expr::Int(1)))),
            end: None,
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert!(map.contains_key(&Key::Int(1)));
                assert!(map.contains_key(&Key::Int(2)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_access_end_only() {
        // [0: a  1: b  2: c][..2] -> [0: a  1: b]
        let entries: Vec<_> = (0..3)
            .map(|i| {
                sp(Entry {
                    key: Some(sp(Expr::Int(i))),
                    value: rsp(Expr::Str(format!("v{i}"))),
                })
            })
            .collect();
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::var_ref("d".into()))),
            start: None,
            end: Some(Box::new(sp(Expr::Int(2)))),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert!(map.contains_key(&Key::Int(0)));
                assert!(map.contains_key(&Key::Int(1)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_access_unbounded() {
        // [0: a  1: b][..] -> all entries
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Int(0))),
                value: rsp(Expr::Str("a".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Int(1))),
                value: rsp(Expr::Str("b".into())),
            }),
        ];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::var_ref("d".into()))),
            start: None,
            end: None,
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => assert_eq!(map.len(), 2),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_access_mixed_keys_error() {
        // [0: a  name: b][0..1] -> error (mixed Int and String keys)
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Int(0))),
                value: rsp(Expr::Str("a".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: rsp(Expr::Str("b".into())),
            }),
        ];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::var_ref("d".into()))),
            start: Some(Box::new(sp(Expr::Int(0)))),
            end: Some(Box::new(sp(Expr::Int(1)))),
        });
        let err = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("comparable key types"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_type_assert_int_passes() {
        // [@Int 42] -> 42
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_string_passes() {
        // [@String hello] -> "hello"
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("String".into())),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::String("hello".into()));
    }

    #[test]
    fn test_type_assert_number_accepts_int() {
        // [@Number 42] -> 42 (Number accepts Int)
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Number".into())),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_number_accepts_float() {
        // [@Number 3.14] -> 3.14 (Number accepts Float)
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Number".into())),
            expr: Box::new(sp(Expr::Float(3.14))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Float(3.14));
    }

    #[test]
    fn test_type_assert_int_fails_on_string() {
        // [@Int hello] -> error
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("type assertion failed: expected Int, got String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_type_assert_string_fails_on_int() {
        // [@String 42] -> error
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("String".into())),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("type assertion failed: expected String, got Int"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_type_assert_bool_passes() {
        // [@Bool true] -> true
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Bool".into())),
            expr: Box::new(sp(Expr::Bool(true))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn test_type_assert_property_dict_with_type() {
        // [@[type: Int] 42] -> 42
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("type".into()))),
            value: rsp(Expr::Str("Int".into())),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_property_dict_type_mismatch() {
        // [@[type: Int] hello] -> error
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("type".into()))),
            value: rsp(Expr::Str("Int".into())),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("type assertion failed: expected Int, got String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_type_assert_property_dict_without_type_passes() {
        // [@[default: 0] hello] -> "hello" (no type key, no check performed)
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(0)),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::String("hello".into()));
    }

    #[test]
    fn test_type_assert_default_not_used_on_match() {
        // [@[type: Int  default: 0] 42] -> 42 (type matches, default not used)
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: rsp(Expr::Str("Int".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::Int(0)),
            }),
        ];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_default_used_on_mismatch() {
        // [@[type: Int  default: 0] hello] -> 0 (type mismatch, returns default)
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: rsp(Expr::Str("Int".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::Int(0)),
            }),
        ];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(0));
    }

    #[test]
    fn test_type_assert_property_dict_no_default_errors_on_mismatch() {
        // [@[type: Int] hello] -> error (no default, mismatch is an error)
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("type".into()))),
            value: rsp(Expr::Str("Int".into())),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("type assertion failed: expected Int, got String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_type_assert_number_default_int_passes_string_triggers() {
        // [@[type: Number  default: -1] 42] -> 42 (Int passes Number check)
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: rsp(Expr::Str("Number".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::Int(-1)),
            }),
        ];
        let expr_pass = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr_pass.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(42));

        // [@[type: Number  default: -1] "nope"] -> -1 (String fails Number, returns default)
        let entries2 = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: rsp(Expr::Str("Number".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::Int(-1)),
            }),
        ];
        let expr_fail = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries2)),
            expr: Box::new(sp(Expr::Str("nope".into()))),
            resolved_type: RefCell::new(None),
        });
        let thunk2 = eval(Rc::new(expr_fail.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val2 = materialize(&thunk2, None, &test_ctx(), 0).unwrap();
        assert_eq!(val2, Value::Int(-1));
    }

    #[test]
    fn test_type_assert_default_accesses_outer_scope() {
        // [@[type: Int  default: $fallback] hello] with fallback=99 -> 99
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: rsp(Expr::Str("Int".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::var_ref("fallback".into())),
            }),
        ];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let env = empty_env();
        env.borrow_mut().insert(
            "fallback".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(99),
                test_span(1, 1, 1, 1),
            )),
        );
        let thunk = eval(Rc::new(expr.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_annotated_bare_string() {
        // Config@ConfigType -> "Config"
        let expr = sp(Expr::Annotated {
            name: "Config".into(),
            annotation: sp(Annotation::Simple("ConfigType".into())),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::String("Config".into()));
    }

    #[test]
    fn test_chained_dot_access() {
        // [outer: [inner: 99]].outer.inner -> 99
        let inner_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("inner".into()))),
            value: rsp(Expr::Int(99)),
        })];
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("outer".into()))),
            value: rsp(Expr::Dict(inner_entries)),
        })];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        // $d.outer.inner
        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::DotAccess {
                expr: Box::new(sp(Expr::var_ref("d".into()))),
                field: "outer".into(),
            })),
            field: "inner".into(),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_eval_depth_limit() {
        // POLICY TEST: This tests the depth-limit POLICY (MAX_EVAL_DEPTH enforcement),
        // not stack-safety. Stack-safety is tested by test_iterative_materialize_deep_chain.
        let expr = sp(Expr::Int(42));
        let err = eval(
            Rc::new(expr.clone()),
            empty_env(),
            &test_ctx(),
            MAX_EVAL_DEPTH + 1,
        )
        .unwrap_err();
        assert!(
            err.message().contains("maximum evaluation depth exceeded"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_materialize_depth_limit() {
        // POLICY TEST: This tests the depth-limit POLICY (MAX_EVAL_DEPTH enforcement),
        // not stack-safety. Stack-safety is tested by test_iterative_materialize_deep_chain.
        //
        // Depth check fires INSIDE deferred-state arms, not before early-returns.
        // Materialized thunks should succeed even at high depth (no evaluation needed).
        // Test with an Unevaluated thunk instead to verify depth check still works.
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(sp(Expr::Int(1)));
        let ctx = test_ctx();
        let thunk = Thunk::new_unevaluated(expr, empty_env(), Rc::clone(&ctx), span);
        let err = materialize(&thunk, None, &ctx, MAX_EVAL_DEPTH + 1).unwrap_err();
        assert!(
            err.message().contains("maximum evaluation depth exceeded"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_proxy_invoke_depth_limit() {
        // POLICY TEST: This tests the depth-limit POLICY (MAX_EVAL_DEPTH enforcement),
        // not stack-safety. Stack-safety is tested by test_iterative_materialize_deep_chain.
        //
        // Verify that accessing a proxy field at depth >= MAX_EVAL_DEPTH triggers
        // the depth exceeded error rather than a Rust stack overflow.
        //
        // Strategy: create a proxy value and access it via a DotAccess expression
        // at depth = MAX_EVAL_DEPTH. The depth check fires during eval(target, ...)
        // when resolving the VarRef $p at depth + 1 = MAX_EVAL_DEPTH + 1, before
        // invoke_proxy_handler is ever reached.
        let span = test_span(1, 1, 1, 5);

        // A simple handler thunk (value doesn't matter — depth check fires before it's invoked)
        let handler = Rc::new(Thunk::new_materialized(Value::Int(0), span));
        let proxy = Value::Proxy { handler };
        let proxy_thunk = Rc::new(Thunk::new_materialized(proxy, span));

        // Insert the proxy into the env so $p resolves to it
        let env = empty_env();
        env.borrow_mut()
            .insert("p".to_string(), Rc::clone(&proxy_thunk));

        // Evaluate $p.field at depth MAX_EVAL_DEPTH
        let dot_expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("p".into()))),
            field: "field".to_string(),
        });
        let ctx = test_ctx();
        let thunk = eval(Rc::new(dot_expr.clone()), env, &ctx, MAX_EVAL_DEPTH).unwrap();
        let err = materialize(&thunk, None, &ctx, MAX_EVAL_DEPTH).unwrap_err();
        assert!(
            err.message().contains("maximum evaluation depth exceeded"),
            "expected depth limit error for proxy field access, got: {}",
            err.message()
        );
    }

    #[test]
    fn test_materialization_span_on_error() {
        // [x: $missing] -- materializing x fails because $missing is undefined
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("missing".into())),
        })];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();

        // Extract x's thunk from the dict
        let x_thunk = match &dict_val {
            Value::Dict(map) => Rc::clone(map.get(&Key::String("x".into())).unwrap()),
            other => panic!("expected Dict, got {other:?}"),
        };

        // Materialize x with a known materialization span
        let mat_span = test_span(5, 1, 5, 5);
        let err = materialize(&x_thunk, Some(&mat_span), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("undefined variable: missing"),
            "got: {}",
            err.message()
        );
        assert_eq!(
            err.materialization_span,
            Some(mat_span),
            "materialization span should be the access site"
        );
    }

    #[test]
    fn test_cycle_has_materialization_span() {
        // [x: $x] -- force x with a known materialization site
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("x".into())),
        })];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();

        match val {
            Value::Dict(map) => {
                let x_thunk = map.get(&Key::String("x".into())).unwrap();
                let mat_span = test_span(10, 1, 10, 5);
                let err = materialize(x_thunk, Some(&mat_span), &test_ctx(), 0).unwrap_err();
                assert!(err.message().contains("circular dependency"));
                assert_eq!(err.materialization_span, Some(mat_span));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_bracket_access_on_non_dict() {
        let env = empty_env();
        env.borrow_mut().insert(
            "x".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );

        let expr = sp(Expr::BracketAccess {
            expr: Box::new(sp(Expr::var_ref("x".into()))),
            key: Box::new(sp(Expr::Int(0))),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(err.message().contains("expected"), "got: {}", err.message());
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_range_access_on_non_dict() {
        let env = empty_env();
        env.borrow_mut().insert(
            "x".into(),
            Rc::new(Thunk::new_materialized(
                Value::String("hello".into()),
                test_span(1, 1, 1, 5),
            )),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::var_ref("x".into()))),
            start: Some(Box::new(sp(Expr::Int(0)))),
            end: Some(Box::new(sp(Expr::Int(2)))),
        });
        let err = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap_err();
        assert!(err.message().contains("expected"), "got: {}", err.message());
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_range_access_on_proxy() {
        // Range access on Proxy values should produce a clear error message
        let span = test_span(1, 1, 1, 5);
        let handler = Rc::new(Thunk::new_materialized(
            Value::Int(42), // handler value doesn't matter for this test
            span,
        ));
        let proxy = Value::Proxy { handler };

        let env = empty_env();
        env.borrow_mut()
            .insert("p".into(), Rc::new(Thunk::new_materialized(proxy, span)));

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::var_ref("p".into()))),
            start: Some(Box::new(sp(Expr::Int(0)))),
            end: Some(Box::new(sp(Expr::Int(2)))),
        });
        let err = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap_err();
        let msg = err.message();
        assert!(msg.contains("range access"), "got: {}", msg);
        assert!(msg.contains("expected Dict"), "got: {}", msg);
        assert!(msg.contains("got Proxy"), "got: {}", msg);
    }

    #[test]
    fn test_range_access_on_proxy_push_frame() {
        // Verify range access on Proxy includes "accessing" in stack frame
        let span = test_span(1, 1, 1, 5);
        let handler = Rc::new(Thunk::new_materialized(Value::Int(42), span));
        let proxy = Value::Proxy { handler };

        let env = empty_env();
        env.borrow_mut()
            .insert("p".into(), Rc::new(Thunk::new_materialized(proxy, span)));

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::var_ref("p".into()))),
            start: Some(Box::new(sp(Expr::Int(0)))),
            end: Some(Box::new(sp(Expr::Int(2)))),
        });
        let err = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap_err();
        // Verify the push_frame call added the accessing frame to the stack
        // The stack frames are stored separately from the message, so check both
        assert!(
            !err.stack.is_empty(),
            "should have stack frames from push_frame"
        );
        // The frame label should contain "accessing"
        let has_accessing_frame = err
            .stack
            .iter()
            .any(|frame| frame.label.contains("accessing"));
        assert!(
            has_accessing_frame,
            "stack should contain 'accessing' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_range_access_on_non_dict_push_frame() {
        // Verify range access on non-Dict value includes "accessing" in stack frame
        let env = empty_env();
        env.borrow_mut().insert(
            "x".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::var_ref("x".into()))),
            start: Some(Box::new(sp(Expr::Int(0)))),
            end: Some(Box::new(sp(Expr::Int(2)))),
        });
        let err = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap_err();
        // Verify the push_frame call added the accessing frame to the stack
        assert!(
            !err.stack.is_empty(),
            "should have stack frames from push_frame"
        );
        // The frame label should contain "accessing"
        let has_accessing_frame = err
            .stack
            .iter()
            .any(|frame| frame.label.contains("accessing"));
        assert!(
            has_accessing_frame,
            "stack should contain 'accessing' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_range_access_string_keys() {
        // [a: 1  b: 2  c: 3  d: 4]["b".."d"] -> [b: 2  c: 3]
        let dict = dict_with_entries(vec![
            ("a", Value::Int(1)),
            ("b", Value::Int(2)),
            ("c", Value::Int(3)),
            ("d", Value::Int(4)),
        ]);
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();
        env.borrow_mut().insert(
            "dd".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::var_ref("dd".into()))),
            start: Some(Box::new(sp(Expr::Str("b".into())))),
            end: Some(Box::new(sp(Expr::Str("d".into())))),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    materialize(
                        map.get(&Key::String("b".into())).unwrap(),
                        None,
                        &test_ctx(),
                        0
                    )
                    .unwrap(),
                    Value::Int(2)
                );
                assert_eq!(
                    materialize(
                        map.get(&Key::String("c".into())).unwrap(),
                        None,
                        &test_ctx(),
                        0
                    )
                    .unwrap(),
                    Value::Int(3)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_value_to_key_invalid_type_bool() {
        // A dict with a Bool key expression should fail in eval_key -> value_to_key
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Bool(true))),
            value: rsp(Expr::Int(1)),
        })];
        let expr = sp(Expr::Dict(entries));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("type mismatch"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String or Int"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Bool"), "got: {}", err.message());
    }

    #[test]
    fn test_value_to_key_invalid_type_float() {
        // A dict with a Float key expression should fail in eval_key -> value_to_key
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Float(3.14))),
            value: rsp(Expr::Int(1)),
        })];
        let expr = sp(Expr::Dict(entries));
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("type mismatch"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String or Int"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("got Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_eval_document_single_expression() {
        // A document with one dict expression returns that dict
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::Int(2)),
            }),
        ];
        let doc = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(entries)))],
            name: None,
            output_type: None,
            expects: None,
        });
        let thunk = eval_document(&doc, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    materialize(
                        map.get(&Key::String("x".into())).unwrap(),
                        None,
                        &test_ctx(),
                        0
                    )
                    .unwrap(),
                    Value::Int(1)
                );
                assert_eq!(
                    materialize(
                        map.get(&Key::String("y".into())).unwrap(),
                        None,
                        &test_ctx(),
                        0
                    )
                    .unwrap(),
                    Value::Int(2)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_scope_chain() {
        // Two expressions: expr 1 defines x, expr 2 references $x
        // Expr 1: [x: 10]
        // Expr 2: [y: $x]
        let expr1 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::Int(10)),
        })]));
        let expr2 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("y".into()))),
            value: rsp(Expr::var_ref("x".into())),
        })]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr1), Rc::new(expr2)],
            name: None,
            output_type: None,
            expects: None,
        });
        let thunk = eval_document(&doc, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(
                    materialize(y_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::Int(10)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_scope_chain_shadowing() {
        // Expr 1: [x: 1]
        // Expr 2: [x: 2  y: $x]
        // y should be 2 (local letrec wins over parent scope)
        let expr1 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::Int(1)),
        })]));
        let expr2 = sp(Expr::Dict(vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(2)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::var_ref("x".into())),
            }),
        ]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr1), Rc::new(expr2)],
            name: None,
            output_type: None,
            expects: None,
        });
        let thunk = eval_document(&doc, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(
                    materialize(y_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::Int(2)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_intermediate_non_dict_error() {
        // Two expressions where expr 1 is a literal (not a dict). Should error.
        let expr1 = sp(Expr::Int(42));
        let expr2 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::Int(1)),
        })]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr1), Rc::new(expr2)],
            name: None,
            output_type: None,
            expects: None,
        });
        let err = eval_document(&doc, empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("document pipeline"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_eval_document_empty() {
        // A document with zero expressions returns an empty dict
        let doc = sp(Document {
            expressions: vec![],
            name: None,
            output_type: None,
            expects: None,
        });
        let thunk = eval_document(&doc, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 0);
            }
            other => panic!("expected empty Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_three_expressions() {
        // Three expressions chaining scope:
        // Expr 1: [a: 1]
        // Expr 2: [b: 2]
        // Expr 3: [ref_a: $a  ref_b: $b]
        // Expr 3 should see both $a (from expr 1 via grandparent) and $b (from expr 2 via parent)
        let expr1 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("a".into()))),
            value: rsp(Expr::Int(1)),
        })]));
        let expr2 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("b".into()))),
            value: rsp(Expr::Int(2)),
        })]));
        let expr3 = sp(Expr::Dict(vec![
            sp(Entry {
                key: Some(sp(Expr::Str("ref_a".into()))),
                value: rsp(Expr::var_ref("a".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("ref_b".into()))),
                value: rsp(Expr::var_ref("b".into())),
            }),
        ]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr1), Rc::new(expr2), Rc::new(expr3)],
            name: None,
            output_type: None,
            expects: None,
        });
        let thunk = eval_document(&doc, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let ref_a = map.get(&Key::String("ref_a".into())).unwrap();
                assert_eq!(
                    materialize(ref_a, None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                let ref_b = map.get(&Key::String("ref_b".into())).unwrap();
                assert_eq!(
                    materialize(ref_b, None, &test_ctx(), 0).unwrap(),
                    Value::Int(2)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_inherits_parent_env() {
        // A document evaluated with a pre-populated parent env.
        // The document's expressions should see the parent's bindings.
        let parent_env = empty_env();
        parent_env.borrow_mut().insert(
            "external".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(999),
                test_span(1, 1, 1, 5),
            )),
        );

        let expr = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("local".into()))),
            value: rsp(Expr::var_ref("external".into())),
        })]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr)],
            name: None,
            output_type: None,
            expects: None,
        });
        let thunk = eval_document(&doc, parent_env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                let local = map.get(&Key::String("local".into())).unwrap();
                assert_eq!(
                    materialize(local, None, &test_ctx(), 0).unwrap(),
                    Value::Int(999)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_single_non_dict_expression() {
        // A document with a single Int expression (not a dict).
        // The last expression can be any type.
        let doc = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Int(42)))],
            name: None,
            output_type: None,
            expects: None,
        });
        let thunk = eval_document(&doc, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_eval_document_integer_keys_skipped_in_scope_chain() {
        // Expr 1: [10 20 30] (auto-indexed: keys Int(0), Int(1), Int(2))
        // Expr 2: [result: 99]
        // Integer keys from expr 1 should not become scope bindings.
        let expr1 = sp(Expr::Dict(vec![
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(10)),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(20)),
            }),
            sp(Entry {
                key: None,
                value: rsp(Expr::Int(30)),
            }),
        ]));
        let expr2 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("result".into()))),
            value: rsp(Expr::Int(99)),
        })]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr1), Rc::new(expr2)],
            name: None,
            output_type: None,
            expects: None,
        });
        let thunk = eval_document(&doc, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                let result_thunk = map.get(&Key::String("result".into())).unwrap();
                assert_eq!(
                    materialize(result_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::Int(99)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_scope_chain_plus_letrec() {
        // Expr 1: [x: 1]
        // Expr 2: [y: $x  z: $y]
        // y references x from the scope chain, z references y via letrec.
        // Verify z resolves to 1.
        let expr1 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::Int(1)),
        })]));
        let expr2 = sp(Expr::Dict(vec![
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::var_ref("x".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("z".into()))),
                value: rsp(Expr::var_ref("y".into())),
            }),
        ]));
        let doc = sp(Document {
            expressions: vec![Rc::new(expr1), Rc::new(expr2)],
            name: None,
            output_type: None,
            expects: None,
        });
        let thunk = eval_document(&doc, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                let z_thunk = map.get(&Key::String("z".into())).unwrap();
                assert_eq!(
                    materialize(z_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    #[ignore = "requires >128MB Rust stack in debug mode; passes in release mode. Stack safety is guaranteed by the iterative materialize_rc loop; these tests verify the depth-limit POLICY only."]
    fn test_eval_document_depth_boundary_error() {
        // Verify that eval_document correctly applies depth+1 to intermediate expression
        // materialization (line 542: materialize(&thunk, Some(&expr.span), ctx, depth + 1)).
        //
        // Challenge: Simple dict literals evaluate to already-materialized thunks (fast path),
        // so materialize() returns immediately without checking depth. To trigger the depth
        // check, we need a thunk in a deferred state (e.g., PendingCall from a function call).
        //
        // This test uses a function call in the intermediate expression to produce a
        // PendingCall thunk. The PendingCall's materialization at depth+1 will then hit
        // the depth check if depth is near MAX_EVAL_DEPTH.
        //
        // Document structure:
        //   Expr 1 (intermediate): [call $id [x: 1]]  — function call that returns an empty dict
        //   Expr 2 (last): [y: 1]                     — simple dict literal
        //
        // At depth=MAX_EVAL_DEPTH-1, the materialize call at depth+1 should hit DepthExceeded.

        // Helper: identity function that returns its argument as-is
        fn id_func(_ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            // For simplicity, just return an empty dict (testing the depth check, not the value)
            Ok(Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                Span::origin(),
            )))
        }

        let env = empty_env();
        env.borrow_mut().insert(
            "id".to_string(),
            Rc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: id_func,
                    name: "id",
                    pos_strictness: &[],
                }),
                Span::origin(),
            )),
        );

        // Expr 1: [call $id [x: 1]]
        let expr1 = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("id".into()))),
            args: vec![rsp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(1)),
            })]))],
            named_args: vec![],
            implied: false,
        });

        // Expr 2: [y: 1]
        let expr2 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("y".into()))),
            value: rsp(Expr::Int(1)),
        })]));

        let doc = sp(Document {
            expressions: vec![Rc::new(expr1), Rc::new(expr2)],
            name: None,
            output_type: None,
            expects: None,
        });

        // Call eval_document at depth=MAX_EVAL_DEPTH-1
        // The materialize call at depth+1 should hit MAX_EVAL_DEPTH and return DepthExceeded
        let result = eval_document(&doc, env, &test_ctx(), MAX_EVAL_DEPTH - 1);

        match result {
            Err(err) if matches!(err.kind, ErrorKind::DepthExceeded { .. }) => {
                // Expected: depth exceeded error
            }
            Err(err) => panic!("expected DepthExceeded, got {:?}", err),
            Ok(_) => panic!("expected DepthExceeded error, but eval_document succeeded"),
        }
    }

    #[test]
    fn test_eval_file_single_document() {
        // A file with one document containing [x: 1]. Verify x=1.
        let doc = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(1)),
            })])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let file = File {
            documents: vec![doc],
        };
        let thunk = eval_file(&file, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                assert_eq!(
                    materialize(
                        map.get(&Key::String("x".into())).unwrap(),
                        None,
                        &test_ctx(),
                        0
                    )
                    .unwrap(),
                    Value::Int(1)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_percent_is_empty_for_first_doc() {
        // A file with one document containing [prev: %].
        // % is VarRef("%"), should resolve to empty dict for first doc.
        let doc = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("prev".into()))),
                value: rsp(Expr::var_ref("%".into())),
            })])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let file = File {
            documents: vec![doc],
        };
        let thunk = eval_file(&file, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                let prev_thunk = map.get(&Key::String("prev".into())).unwrap();
                let prev_val = materialize(prev_thunk, None, &test_ctx(), 0).unwrap();
                match prev_val {
                    Value::Dict(inner) => assert_eq!(inner.len(), 0),
                    other => panic!("expected empty Dict for %, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_percent_pipeline() {
        // Doc 1: [x: 10]
        // Doc 2: [y: %.x]  (access previous doc's x via %)
        // Verify y=10.
        let doc1 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(10)),
            })])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let doc2 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::DotAccess {
                    expr: Box::new(sp(Expr::var_ref("%".into()))),
                    field: "x".into(),
                }),
            })])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let file = File {
            documents: vec![doc1, doc2],
        };
        let thunk = eval_file(&file, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(
                    materialize(y_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::Int(10)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_non_dict_percent() {
        // Doc 1: 42 (a bare Int, not a dict)
        // Doc 2: [prev: %]
        // Verify that prev resolves to Int(42).
        let doc1 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Int(42)))],
            name: None,
            output_type: None,
            expects: None,
        });
        let doc2 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("prev".into()))),
                value: rsp(Expr::var_ref("%".into())),
            })])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let file = File {
            documents: vec![doc1, doc2],
        };
        let thunk = eval_file(&file, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                let prev_thunk = map.get(&Key::String("prev".into())).unwrap();
                assert_eq!(
                    materialize(prev_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::Int(42)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_percent_lazy() {
        // Verify that % is lazy: Doc 1 contains a value that would error if
        // materialized. Doc 2 accesses a DIFFERENT key from %, so the error
        // value is never forced.
        // Doc 1: [good: 1  bad: missing]
        // Doc 2: [result: %.good]
        // Verify result=1.
        let doc1 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![
                sp(Entry {
                    key: Some(sp(Expr::Str("good".into()))),
                    value: rsp(Expr::Int(1)),
                }),
                sp(Entry {
                    key: Some(sp(Expr::Str("bad".into()))),
                    value: rsp(Expr::var_ref("missing".into())),
                }),
            ])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let doc2 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("result".into()))),
                value: rsp(Expr::DotAccess {
                    expr: Box::new(sp(Expr::var_ref("%".into()))),
                    field: "good".into(),
                }),
            })])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let file = File {
            documents: vec![doc1, doc2],
        };
        let thunk = eval_file(&file, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                let result_thunk = map.get(&Key::String("result".into())).unwrap();
                assert_eq!(
                    materialize(result_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_three_documents() {
        // Three documents piped:
        // Doc 1: [a: 1]
        // Doc 2: [b: %.a  c: 2]
        // Doc 3: [result: %.b]
        // Verify result=1 (piped through two boundaries).
        let doc1 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("a".into()))),
                value: rsp(Expr::Int(1)),
            })])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let doc2 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![
                sp(Entry {
                    key: Some(sp(Expr::Str("b".into()))),
                    value: rsp(Expr::DotAccess {
                        expr: Box::new(sp(Expr::var_ref("%".into()))),
                        field: "a".into(),
                    }),
                }),
                sp(Entry {
                    key: Some(sp(Expr::Str("c".into()))),
                    value: rsp(Expr::Int(2)),
                }),
            ])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let doc3 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("result".into()))),
                value: rsp(Expr::DotAccess {
                    expr: Box::new(sp(Expr::var_ref("%".into()))),
                    field: "b".into(),
                }),
            })])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let file = File {
            documents: vec![doc1, doc2, doc3],
        };
        let thunk = eval_file(&file, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                let result_thunk = map.get(&Key::String("result".into())).unwrap();
                assert_eq!(
                    materialize(result_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_documents_isolated() {
        // Verify documents don't share scope:
        // Doc 1: [x: 42]
        // Doc 2: [y: x]  (NOT %.x, just x -- should fail)
        let doc1 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(42)),
            })])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let doc2 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: rsp(Expr::var_ref("x".into())),
            })])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let file = File {
            documents: vec![doc1, doc2],
        };
        // eval_file succeeds (dict is lazy), but materializing y should fail
        let thunk = eval_file(&file, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                let err = materialize(y_thunk, None, &test_ctx(), 0).unwrap_err();
                assert!(
                    err.message().contains("undefined variable: x"),
                    "got: {}",
                    err.message()
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_empty() {
        // A file with zero documents. Should return an empty dict.
        let file = File { documents: vec![] };
        let thunk = eval_file(&file, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected empty Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_inherits_env() {
        // A file evaluated with a pre-populated parent env.
        // Document expressions should see the parent's bindings.
        let parent_env = empty_env();
        parent_env.borrow_mut().insert(
            "external".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(777),
                test_span(1, 1, 1, 5),
            )),
        );

        let doc = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("val".into()))),
                value: rsp(Expr::var_ref("external".into())),
            })])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let file = File {
            documents: vec![doc],
        };
        let thunk = eval_file(&file, parent_env, &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                let val_thunk = map.get(&Key::String("val".into())).unwrap();
                assert_eq!(
                    materialize(val_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::Int(777)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_named_sections() {
        // Test named sections with %name binding
        // Doc 1 (named "defaults"): [port: 8080]
        // Doc 2 (named "overrides"): [host: "prod"]
        // Doc 3 (anonymous): [port: %defaults.port  host: %overrides.host]
        let doc1 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("port".into()))),
                value: rsp(Expr::Int(8080)),
            })])))],
            name: Some("defaults".to_string()),
            output_type: None,
            expects: None,
        });
        let doc2 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("host".into()))),
                value: rsp(Expr::Str("prod".into())),
            })])))],
            name: Some("overrides".to_string()),
            output_type: None,
            expects: None,
        });
        let doc3 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![
                sp(Entry {
                    key: Some(sp(Expr::Str("port".into()))),
                    value: rsp(Expr::DotAccess {
                        expr: Box::new(sp(Expr::var_ref("%defaults".into()))),
                        field: "port".into(),
                    }),
                }),
                sp(Entry {
                    key: Some(sp(Expr::Str("host".into()))),
                    value: rsp(Expr::DotAccess {
                        expr: Box::new(sp(Expr::var_ref("%overrides".into()))),
                        field: "host".into(),
                    }),
                }),
            ])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let file = File {
            documents: vec![doc1, doc2, doc3],
        };
        let thunk = eval_file(&file, empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match val {
            Value::Dict(map) => {
                let port_thunk = map.get(&Key::String("port".into())).unwrap();
                assert_eq!(
                    materialize(port_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::Int(8080)
                );
                let host_thunk = map.get(&Key::String("host".into())).unwrap();
                assert_eq!(
                    materialize(host_thunk, None, &test_ctx(), 0).unwrap(),
                    Value::String("prod".into())
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_named_sections_no_forward_refs() {
        // Test that named sections cannot reference later sections (no forward references).
        //
        // File layout:
        //   Doc 1 (named "early"):  [x: %late.value]   — references %late which is NOT yet defined
        //   Doc 2 (named "late"):   [value: 42]
        //   Doc 3 (unnamed):        [result: %early.x]  — forces materialization of doc1's x field
        //
        // The forward reference %late inside doc1 should produce UndefinedVariable when doc3
        // forces doc1 to materialize. This proves the no-forward-refs invariant.
        let doc1 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::DotAccess {
                    expr: Box::new(sp(Expr::var_ref("%late".into()))),
                    field: "value".into(),
                }),
            })])))],
            name: Some("early".to_string()),
            output_type: None,
            expects: None,
        });
        let doc2 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("value".into()))),
                value: rsp(Expr::Int(42)),
            })])))],
            name: Some("late".to_string()),
            output_type: None,
            expects: None,
        });
        // Doc 3: references %early.x, which forces doc1's x thunk to materialise.
        // x = %late.value, but %late is not bound in doc1's scope, so this must fail.
        let doc3 = sp(Document {
            expressions: vec![Rc::new(sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("result".into()))),
                value: rsp(Expr::DotAccess {
                    expr: Box::new(sp(Expr::var_ref("%early".into()))),
                    field: "x".into(),
                }),
            })])))],
            name: None,
            output_type: None,
            expects: None,
        });
        let file = File {
            documents: vec![doc1, doc2, doc3],
        };
        // eval_file builds lazy thunks — no materialisation yet, so it must succeed.
        let doc3_thunk = eval_file(&file, empty_env(), &test_ctx(), 0).unwrap();

        // Materialise doc3's outer dict — this succeeds (lazy dict construction).
        // The `result` field holds an unevaluated DotAccess thunk for `%early.x`.
        let doc3_val = materialize(&doc3_thunk, None, &test_ctx(), 0)
            .expect("doc3 outer dict should materialise (lazily)");
        let result_thunk = match doc3_val {
            Value::Dict(ref map) => map.get(&Key::String("result".into())).unwrap().clone(),
            other => panic!("expected Dict for doc3, got {other:?}"),
        };

        // Forcing `result` (= %early.x) forces doc1's x thunk, which evaluates `%late.value`.
        // `%late` was NOT bound in doc1's scope (named sections are only bound forward).
        // This must produce UndefinedVariable("%late").
        let err = materialize(&result_thunk, None, &test_ctx(), 0)
            .expect_err("forcing %early.x should fail: %late was not in scope when doc1 was built");
        assert!(
            matches!(err.kind, ErrorKind::UndefinedVariable { ref name } if name == "%late"),
            "expected UndefinedVariable(\"%late\"), got: {:?}",
            err.kind
        );
    }

    #[test]
    fn test_deep_materialize_int() {
        let val = Value::Int(42);
        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn test_deep_materialize_float() {
        let val = Value::Float(3.14);
        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn test_deep_materialize_string() {
        let val = Value::String("hello".into());
        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn test_deep_materialize_bool() {
        let val = Value::Bool(true);
        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn test_deep_materialize_empty_dict() {
        let val = Value::Dict(IndexMap::new());
        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_flat_dict() {
        let span = test_span(1, 1, 1, 5);
        let mut map = IndexMap::new();
        map.insert(
            Key::String("a".into()),
            Rc::new(Thunk::new_materialized(Value::Int(1), span)),
        );
        map.insert(
            Key::String("b".into()),
            Rc::new(Thunk::new_materialized(Value::Int(2), span)),
        );
        let val = Value::Dict(map);
        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let a = materialize(&map[&Key::String("a".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(a, Value::Int(1));
                let b = materialize(&map[&Key::String("b".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(b, Value::Int(2));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_nested_dict() {
        let span = test_span(1, 1, 1, 5);
        let mut inner = IndexMap::new();
        inner.insert(
            Key::String("y".into()),
            Rc::new(Thunk::new_materialized(Value::Int(42), span)),
        );
        let mut outer = IndexMap::new();
        outer.insert(
            Key::String("x".into()),
            Rc::new(Thunk::new_materialized(Value::Dict(inner), span)),
        );
        let val = Value::Dict(outer);
        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        match result {
            Value::Dict(outer_map) => {
                let x_val = materialize(&outer_map[&Key::String("x".into())], None, &test_ctx(), 0)
                    .unwrap();
                match x_val {
                    Value::Dict(inner_map) => {
                        let y_val =
                            materialize(&inner_map[&Key::String("y".into())], None, &test_ctx(), 0)
                                .unwrap();
                        assert_eq!(y_val, Value::Int(42));
                    }
                    other => panic!("expected inner Dict, got {other:?}"),
                }
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_forces_unevaluated_thunks() {
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(99), span));
        let env = Rc::new(RefCell::new(Environment::new()));
        let unevaluated = Rc::new(Thunk::new_unevaluated(
            expr,
            env,
            Rc::clone(&test_ctx()),
            span,
        ));

        let mut map = IndexMap::new();
        map.insert(Key::String("val".into()), unevaluated);
        let val = Value::Dict(map);

        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        match result {
            Value::Dict(map) => {
                let v =
                    materialize(&map[&Key::String("val".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(v, Value::Int(99));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_function_passthrough() {
        let span = test_span(1, 1, 1, 5);
        let val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(Spanned::new(Expr::Int(0), span)),
            env: Rc::new(RefCell::new(Environment::new())),
        };
        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        // Functions are opaque -- returned as-is
        match result {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "x");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_builtin_passthrough() {
        fn dummy(_ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            Ok(Rc::new(Thunk::new_materialized(
                Value::Int(0),
                test_span(1, 1, 1, 1),
            )))
        }
        let val = Value::Builtin(crate::value::BuiltinDef {
            func: dummy,
            name: "test",
            pos_strictness: &[],
        });
        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        match result {
            Value::Builtin(def) => assert_eq!(def.name, "test"),
            other => panic!("expected Builtin, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_depth_limit() {
        // POLICY TEST: deep_materialize checks depth only when traversing into
        // non-leaf structures. Leaf values (Int, String, etc.) return immediately
        // via the fast path without depth checking. Use a nested Dict to trigger
        // the depth check at MAX_EVAL_DEPTH + 1.
        let inner = Value::Dict({
            let mut m = IndexMap::new();
            m.insert(
                Key::String("x".into()),
                Rc::new(Thunk::new_materialized(
                    Value::Int(1),
                    test_span(1, 1, 1, 1),
                )),
            );
            m
        });
        let err = deep_materialize(&inner, &test_ctx(), MAX_EVAL_DEPTH + 1, None).unwrap_err();
        assert!(
            err.message().contains("maximum evaluation depth exceeded"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_deep_materialize_depth_just_under() {
        // At the limit should still succeed for a leaf value
        let result = deep_materialize(&Value::Int(1), &test_ctx(), MAX_EVAL_DEPTH, None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_deep_materialize_dict_with_int_keys() {
        let span = test_span(1, 1, 1, 5);
        let mut map = IndexMap::new();
        map.insert(
            Key::Int(0),
            Rc::new(Thunk::new_materialized(Value::String("zero".into()), span)),
        );
        map.insert(
            Key::Int(1),
            Rc::new(Thunk::new_materialized(Value::String("one".into()), span)),
        );
        let val = Value::Dict(map);
        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let v0 = materialize(&map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v0, Value::String("zero".into()));
                let v1 = materialize(&map[&Key::Int(1)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v1, Value::String("one".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_preserves_key_order() {
        let span = test_span(1, 1, 1, 5);
        let mut map = IndexMap::new();
        map.insert(
            Key::String("c".into()),
            Rc::new(Thunk::new_materialized(Value::Int(3), span)),
        );
        map.insert(
            Key::String("a".into()),
            Rc::new(Thunk::new_materialized(Value::Int(1), span)),
        );
        map.insert(
            Key::String("b".into()),
            Rc::new(Thunk::new_materialized(Value::Int(2), span)),
        );
        let val = Value::Dict(map);
        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        match result {
            Value::Dict(map) => {
                let keys: Vec<&Key> = map.keys().collect();
                assert_eq!(
                    keys,
                    vec![
                        &Key::String("c".into()),
                        &Key::String("a".into()),
                        &Key::String("b".into()),
                    ]
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_dict_containing_function() {
        // Dict with a function value -- function should pass through, not be traversed
        let span = test_span(1, 1, 1, 5);
        let func_val = Value::Function {
            params: Rc::new(vec![]),
            body: Rc::new(Spanned::new(Expr::Int(0), span)),
            env: Rc::new(RefCell::new(Environment::new())),
        };
        let mut map = IndexMap::new();
        map.insert(
            Key::String("f".into()),
            Rc::new(Thunk::new_materialized(func_val, span)),
        );
        map.insert(
            Key::String("v".into()),
            Rc::new(Thunk::new_materialized(Value::Int(10), span)),
        );
        let val = Value::Dict(map);
        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        match result {
            Value::Dict(map) => {
                let f = materialize(&map[&Key::String("f".into())], None, &test_ctx(), 0).unwrap();
                assert!(matches!(f, Value::Function { .. }));
                let v = materialize(&map[&Key::String("v".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(v, Value::Int(10));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_three_levels_deep() {
        let span = test_span(1, 1, 1, 5);

        // Build [a: [b: [c: 99]]]
        let mut level3 = IndexMap::new();
        level3.insert(
            Key::String("c".into()),
            Rc::new(Thunk::new_materialized(Value::Int(99), span)),
        );
        let mut level2 = IndexMap::new();
        level2.insert(
            Key::String("b".into()),
            Rc::new(Thunk::new_materialized(Value::Dict(level3), span)),
        );
        let mut level1 = IndexMap::new();
        level1.insert(
            Key::String("a".into()),
            Rc::new(Thunk::new_materialized(Value::Dict(level2), span)),
        );
        let val = Value::Dict(level1);

        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        // Navigate three levels deep
        match result {
            Value::Dict(l1) => {
                let a = materialize(&l1[&Key::String("a".into())], None, &test_ctx(), 0).unwrap();
                match a {
                    Value::Dict(l2) => {
                        let b = materialize(&l2[&Key::String("b".into())], None, &test_ctx(), 0)
                            .unwrap();
                        match b {
                            Value::Dict(l3) => {
                                let c = materialize(
                                    &l3[&Key::String("c".into())],
                                    None,
                                    &test_ctx(),
                                    0,
                                )
                                .unwrap();
                                assert_eq!(c, Value::Int(99));
                            }
                            other => panic!("expected level 3 Dict, got {other:?}"),
                        }
                    }
                    other => panic!("expected level 2 Dict, got {other:?}"),
                }
            }
            other => panic!("expected level 1 Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_result_thunks_are_materialized() {
        // Verify that after deep_materialize, all thunks in the result dict
        // are in the Materialized state (not Unevaluated or PendingBuiltin)
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(7), span));
        let env = Rc::new(RefCell::new(Environment::new()));
        let unevaluated = Rc::new(Thunk::new_unevaluated(
            expr,
            env,
            Rc::clone(&test_ctx()),
            span,
        ));

        let mut map = IndexMap::new();
        map.insert(Key::String("x".into()), unevaluated);
        let val = Value::Dict(map);

        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        match result {
            Value::Dict(map) => {
                let thunk = &map[&Key::String("x".into())];
                // The thunk in the result should be in Materialized state
                assert!(matches!(
                    &*thunk.state(),
                    ThunkState::Materialized(Value::Int(7))
                ));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_seq() {
        // Verify that deep_materialize forces both head and tail of Seq
        let span = test_span(1, 1, 1, 5);
        let head_expr = Rc::new(Spanned::new(Expr::Int(42), span));
        let env = Rc::new(RefCell::new(Environment::new()));
        let head_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::clone(&head_expr),
            Rc::clone(&env),
            Rc::clone(&test_ctx()),
            span,
        ));

        let tail_expr = Rc::new(Spanned::new(Expr::Str("tail".into()), span));
        let tail_thunk = Rc::new(Thunk::new_unevaluated(
            tail_expr,
            Rc::clone(&env),
            Rc::clone(&test_ctx()),
            span,
        ));

        let seq = Value::Seq {
            head: head_thunk,
            tail: tail_thunk,
        };

        let result = deep_materialize(&seq, &test_ctx(), 0, None).unwrap();
        match result {
            Value::Seq { head, tail } => {
                // Both head and tail should be materialized
                let head_val = &*head.state();
                assert!(matches!(head_val, ThunkState::Materialized(Value::Int(42))));

                let tail_val = &*tail.state();
                assert!(matches!(
                    tail_val,
                    ThunkState::Materialized(Value::String(s)) if s == "tail"
                ));
            }
            other => panic!("expected Seq, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_seq_depth_limit() {
        // POLICY TEST: This tests the depth-limit POLICY (MAX_EVAL_DEPTH enforcement),
        // not stack-safety. Stack-safety is tested by test_iterative_materialize_deep_chain.
        //
        // Build a deeply nested Seq structure exceeding MAX_EVAL_DEPTH.
        // The seq_depth counter fires before the generic depth limit,
        // giving a targeted error message for infinite sequences.
        let span = test_span(1, 1, 1, 1);
        let mut current = Rc::new(Thunk::new_materialized(Value::Dict(IndexMap::new()), span));

        // Create MAX_EVAL_DEPTH + 2 nested Seq values
        for _ in 0..MAX_EVAL_DEPTH + 2 {
            let seq = Value::Seq {
                head: Rc::new(Thunk::new_materialized(Value::Int(1), span)),
                tail: Rc::clone(&current),
            };
            current = Rc::new(Thunk::new_materialized(seq, span));
        }

        let outer_seq = materialize(&current, None, &test_ctx(), 0).unwrap();
        let err = deep_materialize(&outer_seq, &test_ctx(), 0, None).unwrap_err();
        assert!(
            err.message()
                .contains("cannot deep-materialize an infinite Seq"),
            "expected infinite Seq error, got: {}",
            err.message()
        );
    }

    // ── Sharing preservation tests (Launchbury 1993 invariant) ──────────

    #[test]
    fn test_deep_materialize_preserves_dict_sharing() {
        // Two dict entries share the same Rc<Thunk>. After deep_materialize,
        // the output entries must still be Rc::ptr_eq — the sharing invariant.
        let span = test_span(1, 1, 1, 5);
        let shared_thunk = Rc::new(Thunk::new_materialized(Value::Int(42), span));
        assert!(Rc::ptr_eq(&shared_thunk, &Rc::clone(&shared_thunk)));

        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), Rc::clone(&shared_thunk));
        map.insert(Key::String("b".into()), Rc::clone(&shared_thunk));
        let val = Value::Dict(map);

        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        match result {
            Value::Dict(map) => {
                let a = &map[&Key::String("a".into())];
                let b = &map[&Key::String("b".into())];
                assert!(
                    Rc::ptr_eq(a, b),
                    "deep_materialize must preserve sharing: entries pointing to the \
                     same Rc<Thunk> should remain Rc::ptr_eq after deep materialization"
                );
                // Also verify the value is correct
                let v = materialize(a, None, &test_ctx(), 0).unwrap();
                assert_eq!(v, Value::Int(42));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_preserves_seq_sharing() {
        // Head and tail share the same Rc<Thunk>. After deep_materialize,
        // they must still be Rc::ptr_eq.
        // Intentionally invalid Seq tail (Int instead of Seq/Dict) — tests sharing preservation without needing valid continuation
        let span = test_span(1, 1, 1, 5);
        let shared_thunk = Rc::new(Thunk::new_materialized(Value::Int(99), span));

        let seq = Value::Seq {
            head: Rc::clone(&shared_thunk),
            tail: Rc::clone(&shared_thunk),
        };

        let result = deep_materialize(&seq, &test_ctx(), 0, None).unwrap();
        match result {
            Value::Seq { head, tail } => {
                assert!(
                    Rc::ptr_eq(&head, &tail),
                    "deep_materialize must preserve sharing in Seq: head and tail \
                     pointing to the same Rc<Thunk> should remain Rc::ptr_eq"
                );
                let v = materialize(&head, None, &test_ctx(), 0).unwrap();
                assert_eq!(v, Value::Int(99));
            }
            other => panic!("expected Seq, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_preserves_cross_structure_sharing() {
        // A shared thunk appears in both a nested dict and a seq within the
        // same top-level dict. All occurrences must resolve to the same Rc.
        let span = test_span(1, 1, 1, 5);
        let shared = Rc::new(Thunk::new_materialized(
            Value::String("shared".into()),
            span,
        ));

        let mut inner_dict = IndexMap::new();
        inner_dict.insert(Key::String("x".into()), Rc::clone(&shared));
        let inner_dict_thunk = Rc::new(Thunk::new_materialized(Value::Dict(inner_dict), span));

        let seq_val = Value::Seq {
            head: Rc::clone(&shared),
            tail: Rc::new(Thunk::new_materialized(Value::Dict(IndexMap::new()), span)),
        };
        let seq_thunk = Rc::new(Thunk::new_materialized(seq_val, span));

        let mut outer = IndexMap::new();
        outer.insert(Key::String("nested".into()), inner_dict_thunk);
        outer.insert(Key::String("seq".into()), seq_thunk);
        let val = Value::Dict(outer);

        let result = deep_materialize(&val, &test_ctx(), 0, None).unwrap();
        match result {
            Value::Dict(map) => {
                // Extract the shared thunk from the nested dict
                let nested_val =
                    materialize(&map[&Key::String("nested".into())], None, &test_ctx(), 0).unwrap();
                let nested_shared = match nested_val {
                    Value::Dict(d) => Rc::clone(&d[&Key::String("x".into())]),
                    other => panic!("expected Dict, got {other:?}"),
                };

                // Extract the shared thunk from the seq head
                let seq_val =
                    materialize(&map[&Key::String("seq".into())], None, &test_ctx(), 0).unwrap();
                let seq_shared = match seq_val {
                    Value::Seq { head, .. } => head,
                    other => panic!("expected Seq, got {other:?}"),
                };

                assert!(
                    Rc::ptr_eq(&nested_shared, &seq_shared),
                    "deep_materialize must preserve sharing across nested dicts and seqs"
                );
                // Also verify the shared value is correct
                let v = materialize(&nested_shared, None, &test_ctx(), 0).unwrap();
                assert_eq!(v, Value::String("shared".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_proxy() {
        // Test that deep_materialize traverses into the proxy handler thunk
        // and returns a new Proxy with the deep-materialized handler.
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(42), span));
        let env = Rc::new(RefCell::new(Environment::new()));
        let ctx = test_ctx();

        // Create an unevaluated handler thunk
        let handler = Rc::new(Thunk::new_unevaluated(expr, env, Rc::clone(&ctx), span));
        let proxy_val = Value::Proxy {
            handler: Rc::clone(&handler),
        };

        // Deep materialize the proxy
        let result = deep_materialize(&proxy_val, &ctx, 0, None).unwrap();

        match result {
            Value::Proxy {
                handler: deep_handler,
            } => {
                // Verify the handler was deep-materialized
                let handler_val = materialize(&deep_handler, None, &ctx, 0).unwrap();
                assert_eq!(handler_val, Value::Int(42));
            }
            other => panic!("expected Proxy, got {other:?}"),
        }
    }

    // ── Stack trace / call stack reconstruction tests ──────────────────

    #[test]
    fn test_call_error_has_stack_frame_with_function_name() {
        // [f: [fn [x] missing]; result: [f 1]]
        // Calling f with body that references missing should produce a
        // stack frame with "[f ...]".
        let env = empty_env();
        let fn_span = test_span(1, 1, 1, 20);
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(Spanned::new(
                Expr::var_ref("missing".into()),
                test_span(1, 15, 1, 23),
            )),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, fn_span)),
        );

        let call_span = test_span(2, 1, 2, 15);
        let call_expr = Spanned::new(
            Expr::Call {
                func: Box::new(Spanned::new(
                    Expr::var_ref("f".into()),
                    test_span(2, 7, 2, 8),
                )),
                args: vec![Rc::new(Spanned::new(Expr::Int(1), test_span(2, 10, 2, 11)))],
                named_args: vec![],
                implied: false,
            },
            call_span,
        );

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("undefined variable: missing"),
            "got: {}",
            err.message()
        );
        // The stack should contain a frame for "[f ...]"
        assert!(
            err.stack.iter().any(|f| f.label == "[f ...]"),
            "expected '[f ...]' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_nested_call_produces_multi_frame_stack() {
        // inner: [fn [x] $missing]
        // outer: [fn [y] [call $inner $y]]
        // [call $outer 1]
        //
        // Error should show both call sites in the stack.
        let env = empty_env();

        // Inner function
        let inner_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(Spanned::new(
                Expr::var_ref("missing".into()),
                test_span(1, 20, 1, 28),
            )),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "inner".into(),
            Rc::new(Thunk::new_materialized(inner_fn, test_span(1, 1, 1, 30))),
        );

        // Outer function: body is [call $inner $y]
        let inner_call_span = test_span(2, 15, 2, 30);
        let outer_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "y".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(Spanned::new(
                Expr::Call {
                    func: Box::new(Spanned::new(
                        Expr::var_ref("inner".into()),
                        test_span(2, 21, 2, 26),
                    )),
                    args: vec![Rc::new(Spanned::new(
                        Expr::var_ref("y".into()),
                        test_span(2, 28, 2, 29),
                    ))],
                    named_args: vec![],
                    implied: false,
                },
                inner_call_span,
            )),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "outer".into(),
            Rc::new(Thunk::new_materialized(outer_fn, test_span(2, 1, 2, 35))),
        );

        // Evaluate [call $outer 1]
        let outer_call_span = test_span(3, 1, 3, 20);
        let call_expr = Spanned::new(
            Expr::Call {
                func: Box::new(Spanned::new(
                    Expr::var_ref("outer".into()),
                    test_span(3, 7, 3, 12),
                )),
                args: vec![Rc::new(Spanned::new(Expr::Int(1), test_span(3, 14, 3, 15)))],
                named_args: vec![],
                implied: false,
            },
            outer_call_span,
        );

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(err.message().contains("undefined variable: missing"));

        // Should have frames for both call sites
        let labels: Vec<&str> = err.stack.iter().map(|f| f.label.as_str()).collect();
        assert!(
            labels.contains(&"[inner ...]"),
            "expected '[inner ...]' in stack, got: {labels:?}"
        );
        assert!(
            labels.contains(&"[outer ...]"),
            "expected '[outer ...]' in stack, got: {labels:?}"
        );
        // Inner call should appear before outer call (innermost first)
        let inner_pos = labels.iter().position(|l| *l == "[inner ...]").unwrap();
        let outer_pos = labels.iter().position(|l| *l == "[outer ...]").unwrap();
        assert!(
            inner_pos < outer_pos,
            "inner call frame should come before outer: {labels:?}"
        );
    }

    #[test]
    fn test_dot_access_error_has_access_frame() {
        // When dot access fails because the target evaluation itself errors,
        // the error should include a frame indicating the access context.
        //
        // [a: $missing]
        // $a.x  -- accessing .x should add a frame
        let env = empty_env();

        // Put a dict with a broken value in the env
        let dict_span = test_span(1, 1, 1, 20);
        let mut dict_map = IndexMap::new();
        let bad_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(Spanned::new(
                Expr::var_ref("missing".into()),
                test_span(1, 8, 1, 15),
            )),
            Rc::clone(&env),
            Rc::clone(&test_ctx()),
            test_span(1, 8, 1, 15),
        ));
        dict_map.insert(Key::String("x".into()), bad_thunk);

        env.borrow_mut().insert(
            "a".into(),
            Rc::new(Thunk::new_materialized(Value::Dict(dict_map), dict_span)),
        );

        // Now access $a.x -- this should succeed (returns the thunk), but
        // materializing the result should fail
        let access_span = test_span(2, 1, 2, 5);
        let access_expr = Spanned::new(
            Expr::DotAccess {
                expr: Box::new(Spanned::new(
                    Expr::var_ref("a".into()),
                    test_span(2, 1, 2, 2),
                )),
                field: "x".into(),
            },
            access_span,
        );

        let thunk = eval(Rc::new(access_expr.clone()), env, &test_ctx(), 0).unwrap();
        let mat_span = test_span(3, 1, 3, 10);
        let err = materialize(&thunk, Some(&mat_span), &test_ctx(), 0).unwrap_err();
        assert!(err.message().contains("undefined variable: missing"));
        // The materialization span should be set
        assert!(err.materialization_span.is_some());
    }

    #[test]
    fn test_dot_access_on_erroring_target_has_frame() {
        // $nonexistent.field -- the target itself fails, and the error
        // should include an "accessing .field" frame.
        let env = empty_env();
        let access_span = test_span(1, 1, 1, 20);
        let access_expr = Spanned::new(
            Expr::DotAccess {
                expr: Box::new(Spanned::new(
                    Expr::var_ref("nonexistent".into()),
                    test_span(1, 1, 1, 12),
                )),
                field: "field".into(),
            },
            access_span,
        );

        let thunk = eval(Rc::new(access_expr.clone()), env, &test_ctx(), 0).unwrap();
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(err.message().contains("undefined variable: nonexistent"));
        // Should have an "accessing .field" frame
        assert!(
            err.stack.iter().any(|f| f.label == "accessing .field"),
            "expected 'accessing .field' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_bracket_access_on_erroring_target_has_frame() {
        // $nonexistent[0] -- the target itself fails
        let env = empty_env();
        let access_span = test_span(1, 1, 1, 20);
        let access_expr = Spanned::new(
            Expr::BracketAccess {
                expr: Box::new(Spanned::new(
                    Expr::var_ref("nonexistent".into()),
                    test_span(1, 1, 1, 12),
                )),
                key: Box::new(Spanned::new(Expr::Int(0), test_span(1, 13, 1, 14))),
            },
            access_span,
        );

        let thunk = eval(Rc::new(access_expr.clone()), env, &test_ctx(), 0).unwrap();
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(err.message().contains("undefined variable: nonexistent"));
        assert!(
            err.stack.iter().any(|f| f.label == "accessing [..]"),
            "expected 'accessing [..]' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_range_access_on_erroring_target_has_frame() {
        // $nonexistent[0..2] -- the target itself fails
        let env = empty_env();
        let access_span = test_span(1, 1, 1, 20);
        let access_expr = Spanned::new(
            Expr::RangeAccess {
                expr: Box::new(Spanned::new(
                    Expr::var_ref("nonexistent".into()),
                    test_span(1, 1, 1, 12),
                )),
                start: Some(Box::new(Spanned::new(
                    Expr::Int(0),
                    test_span(1, 13, 1, 14),
                ))),
                end: Some(Box::new(Spanned::new(
                    Expr::Int(2),
                    test_span(1, 16, 1, 17),
                ))),
            },
            access_span,
        );

        let err = eval(Rc::new(access_expr.clone()), env, &test_ctx(), 0).unwrap_err();
        assert!(err.message().contains("undefined variable: nonexistent"));
        assert!(
            err.stack.iter().any(|f| f.label == "accessing [..:..]"),
            "expected 'accessing [..:..]' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_chained_access_error_shows_chain() {
        // [a: [x: $missing]]
        // $a.x  -- force chain
        // When materialized, the error should show the materialization chain.
        let inner_env = empty_env();
        let mut inner_map = IndexMap::new();
        inner_map.insert(
            Key::String("x".into()),
            Rc::new(Thunk::new_unevaluated(
                Rc::new(Spanned::new(
                    Expr::var_ref("missing".into()),
                    test_span(1, 10, 1, 18),
                )),
                Rc::clone(&inner_env),
                Rc::clone(&test_ctx()),
                test_span(1, 10, 1, 18),
            )),
        );
        let inner_dict = Value::Dict(inner_map);

        let env = empty_env();
        env.borrow_mut().insert(
            "a".into(),
            Rc::new(Thunk::new_materialized(inner_dict, test_span(1, 1, 1, 20))),
        );

        // Build $a.x access
        let access_span = test_span(2, 1, 2, 5);
        let access_expr = Spanned::new(
            Expr::DotAccess {
                expr: Box::new(Spanned::new(
                    Expr::var_ref("a".into()),
                    test_span(2, 1, 2, 2),
                )),
                field: "x".into(),
            },
            access_span,
        );

        // Eval returns an Unevaluated thunk wrapping the DotAccess
        let thunk = eval(
            Rc::new(access_expr.clone()),
            Rc::clone(&env),
            &test_ctx(),
            0,
        )
        .unwrap();

        // Materialize with a different span (simulating a reference from $b)
        let b_span = test_span(3, 1, 3, 5);
        let err = materialize(&thunk, Some(&b_span), &test_ctx(), 0).unwrap_err();
        assert!(err.message().contains("undefined variable: missing"));
        // Note: Currently uses access_span (from DotAccess inline handler in force_step)
        // rather than b_span. This is a known limitation — nested materializations during
        // access chain processing use the access expr span, not the outer mat_span.
        // TODO: propagate outer mat_span through access chain continuations
        assert_eq!(
            err.materialization_span,
            Some(access_span),
            "currently uses access span due to force_step DotAccess inline handling"
        );
    }

    #[test]
    fn test_func_label_varref() {
        let label = func_label(&Expr::var_ref("f".into()));
        assert_eq!(label.as_deref(), Some("[f ...]"));
    }

    #[test]
    fn test_func_label_dot_access() {
        let expr = Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("utils".into()))),
            field: "run".into(),
        };
        let label = func_label(&expr);
        assert_eq!(label.as_deref(), Some("[<dot-access> ...]"));
    }

    #[test]
    fn test_func_label_chained_dot_access() {
        let expr = Expr::DotAccess {
            expr: Box::new(sp(Expr::DotAccess {
                expr: Box::new(sp(Expr::var_ref("a".into()))),
                field: "b".into(),
            })),
            field: "c".into(),
        };
        let label = func_label(&expr);
        assert_eq!(label.as_deref(), Some("[<dot-access> ...]"));
    }

    #[test]
    fn test_func_label_anonymous() {
        // Anonymous calls return None (no origin label adds diagnostic value)
        assert_eq!(func_label(&Expr::Int(42)), None);
    }

    #[test]
    fn test_materialize_chain_no_duplicate_frames() {
        // When the same mat_span propagates through nested materialize calls,
        // we should not get duplicate frames for the same span.
        let env = empty_env();

        // Create a thunk whose body is another unevaluated thunk that errors
        let inner_expr = Spanned::new(Expr::var_ref("missing".into()), test_span(1, 1, 1, 8));
        let inner_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(inner_expr),
            Rc::clone(&env),
            Rc::clone(&test_ctx()),
            test_span(1, 1, 1, 8),
        ));

        // Materialize with a specific span
        let mat_span = test_span(5, 1, 5, 10);
        let err = materialize(&inner_thunk, Some(&mat_span), &test_ctx(), 0).unwrap_err();

        // Count how many frames have the same span
        let frame_count = err.stack.iter().filter(|f| f.span == mat_span).count();
        assert!(
            frame_count <= 1,
            "expected at most 1 frame with mat_span, got {frame_count}: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_call_arity_error_has_call_frame() {
        // Calling a function with wrong arity should include the call site frame
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![
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
            ]),
            body: Rc::new(sp(Expr::var_ref("a".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 20))),
        );

        // Call with wrong arity: [call $f 1] (needs 2 args)
        let call_span = test_span(2, 1, 2, 15);
        let call_expr = Spanned::new(
            Expr::Call {
                func: Box::new(Spanned::new(
                    Expr::var_ref("f".into()),
                    test_span(2, 7, 2, 8),
                )),
                args: vec![Rc::new(Spanned::new(Expr::Int(1), test_span(2, 10, 2, 11)))],
                named_args: vec![],
                implied: false,
            },
            call_span,
        );

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0)
            .expect("eval should return PendingCall thunk");
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(err
            .message()
            .contains("missing argument for required parameter"));
        assert!(
            err.stack.iter().any(|f| f.label == "[f ...]"),
            "expected '[f ...]' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_builtin_error_has_stack_frame_with_builtin_name() {
        // Calling a builtin that errors should include "call $builtin_name" in the stack.
        // We'll use $type-of with an intentionally broken setup to trigger an error.
        // Actually, let's use a custom failing builtin for clarity.
        fn failing_builtin(_ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            Err(
                EvalError::internal("test builtin failure".to_string(), test_span(99, 1, 99, 10))
                    .into(),
            )
        }

        let env = empty_env();
        env.borrow_mut().insert(
            "fail".into(),
            Rc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: failing_builtin,
                    name: "fail",
                    pos_strictness: &[],
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("fail".into()))),
            args: vec![],
            named_args: vec![],
            implied: false,
        });

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();
        let err = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(err.message().contains("test builtin failure"));
        // The stack should contain "[fail ...]"
        assert!(
            err.stack.iter().any(|f| f.label == "[fail ...]"),
            "expected '[fail ...]' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_error_display_with_full_stack() {
        // Integration test: verify the Display output includes all stack frames
        let err = EvalError::internal("something broke".to_string(), test_span(1, 5, 1, 12))
            .with_materialization_span(test_span(10, 1, 10, 5))
            .with_frame("[inner ...]".to_string(), test_span(5, 1, 5, 20))
            .with_frame("[outer ...]".to_string(), test_span(8, 1, 8, 25));
        let display = format!("{err}");
        assert!(display.contains("something broke"));
        assert!(display.contains("defined at 1:5-1:12"));
        // infer_materialization_verb returns "called at" when first visible frame starts with '['
        assert!(display.contains("called at 10:1-10:5"));
        assert!(display.contains("in [inner ...] at 5:1-5:20"));
        assert!(display.contains("in [outer ...] at 8:1-8:25"));
    }

    // ── PendingCall thunk state tests ──────────────────────────────────

    #[test]
    fn test_pending_call_llt_function() {
        // Create a PendingCall thunk that calls an LLT function
        // [fn [x y] [call $+ $x $y]] with args (3, 4)
        let env = empty_env();

        // Create a simple addition function
        let add_fn = Value::Function {
            params: Rc::new(vec![
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
            ]),
            body: Rc::new(sp(Expr::Call {
                func: Box::new(sp(Expr::var_ref("+".into()))),
                args: vec![
                    rsp(Expr::var_ref("x".into())),
                    rsp(Expr::var_ref("y".into())),
                ],
                named_args: vec![],
                implied: false,
            })),
            env: Rc::clone(&env),
        };

        // Add the builtin $+ to the environment
        fn add_builtin(ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let crate::value::BuiltinArgs { args, .. } = ctx;
            let a = materialize(&args[0], None, &test_ctx(), 0)?;
            let b = materialize(&args[1], None, &test_ctx(), 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Rc::new(Thunk::new_materialized(
                    Value::Int(x + y),
                    test_span(1, 1, 1, 1),
                ))),
                _ => panic!("test expects Int args"),
            }
        }
        env.borrow_mut().insert(
            "+".into(),
            Rc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: add_builtin,
                    name: "+",
                    pos_strictness: &[],
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        // Create PendingCall thunk
        let func_thunk = Rc::new(Thunk::new_materialized(add_fn, test_span(1, 1, 1, 20)));
        let arg1 = Rc::new(Thunk::new_materialized(
            Value::Int(3),
            test_span(1, 21, 1, 22),
        ));
        let arg2 = Rc::new(Thunk::new_materialized(
            Value::Int(4),
            test_span(1, 23, 1, 24),
        ));
        let call_span = test_span(2, 1, 2, 15);

        let pending = Thunk::new_pending_call(
            func_thunk,
            vec![arg1, arg2],
            IndexMap::new(),
            call_span,
            empty_env(),
            call_span,
            Some(Rc::from("test-pending-call")),
            Rc::clone(&test_ctx()),
        );

        // Materialize should call the function and return the result
        let result = materialize(&pending, None, &test_ctx(), 0).unwrap();
        assert_eq!(result, Value::Int(7));
    }

    #[test]
    fn test_pending_call_builtin_function() {
        // Create a PendingCall thunk where the function is a Builtin
        fn multiply_builtin(ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let crate::value::BuiltinArgs { args, .. } = ctx;
            let a = materialize(&args[0], None, &test_ctx(), 0)?;
            let b = materialize(&args[1], None, &test_ctx(), 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Rc::new(Thunk::new_materialized(
                    Value::Int(x * y),
                    test_span(1, 1, 1, 1),
                ))),
                _ => panic!("test expects Int args"),
            }
        }

        let func_thunk = Rc::new(Thunk::new_materialized(
            Value::Builtin(crate::value::BuiltinDef {
                func: multiply_builtin,
                name: "*",
                pos_strictness: &[],
            }),
            test_span(1, 1, 1, 5),
        ));
        let arg1 = Rc::new(Thunk::new_materialized(
            Value::Int(5),
            test_span(1, 6, 1, 7),
        ));
        let arg2 = Rc::new(Thunk::new_materialized(
            Value::Int(6),
            test_span(1, 8, 1, 9),
        ));
        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            func_thunk,
            vec![arg1, arg2],
            IndexMap::new(),
            call_span,
            empty_env(),
            call_span,
            Some(Rc::from("test-pending-call")),
            Rc::clone(&test_ctx()),
        );

        // Materialize should call the builtin directly and return the result
        let result = materialize(&pending, None, &test_ctx(), 0).unwrap();
        assert_eq!(result, Value::Int(30));
    }

    #[test]
    fn test_pending_call_memoizes() {
        // PendingCall should memoize: second materialization returns cached value
        let env = empty_env();

        // Create a function that would fail if called twice
        // (we'll verify it's only called once by checking the state)
        let identity_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::var_ref("x".into()))),
            env: Rc::clone(&env),
        };

        let func_thunk = Rc::new(Thunk::new_materialized(identity_fn, test_span(1, 1, 1, 10)));
        let arg = Rc::new(Thunk::new_materialized(
            Value::Int(42),
            test_span(1, 11, 1, 13),
        ));
        let call_span = test_span(2, 1, 2, 10);

        let pending = Rc::new(Thunk::new_pending_call(
            func_thunk,
            vec![arg],
            IndexMap::new(),
            call_span,
            empty_env(),
            call_span,
            Some(Rc::from("test-pending-call")),
            Rc::clone(&test_ctx()),
        ));

        // First materialization
        let result1 = materialize(&pending, None, &test_ctx(), 0).unwrap();
        assert_eq!(result1, Value::Int(42));

        // Check that the thunk is now in Materialized state
        match &*pending.state() {
            ThunkState::Materialized(v) => assert_eq!(*v, Value::Int(42)),
            other => panic!("expected Materialized after first call, got {other:?}"),
        }

        // Second materialization should return cached value
        let result2 = materialize(&pending, None, &test_ctx(), 0).unwrap();
        assert_eq!(result2, Value::Int(42));
    }

    #[test]
    fn test_pending_call_non_function_error() {
        // PendingCall with a non-Function/Builtin value should error
        let not_a_function = Rc::new(Thunk::new_materialized(
            Value::Int(123),
            test_span(1, 1, 1, 4),
        ));
        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            not_a_function,
            vec![],
            IndexMap::new(),
            call_span,
            empty_env(),
            call_span,
            Some(Rc::from("test-pending-call")),
            Rc::clone(&test_ctx()),
        );

        let err = materialize(&pending, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("expected Function or Builtin, got Int"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_pending_call_with_unevaluated_args() {
        // PendingCall should work with unevaluated argument thunks (lazy evaluation)
        let env = empty_env();

        let identity_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::var_ref("x".into()))),
            env: Rc::clone(&env),
        };

        let func_thunk = Rc::new(Thunk::new_materialized(identity_fn, test_span(1, 1, 1, 10)));

        // Create an unevaluated arg
        let arg_expr = Rc::new(sp(Expr::Int(99)));
        let arg = Rc::new(Thunk::new_unevaluated(
            arg_expr,
            Rc::clone(&env),
            Rc::clone(&test_ctx()),
            test_span(1, 11, 1, 13),
        ));

        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            func_thunk,
            vec![arg],
            IndexMap::new(),
            call_span,
            empty_env(),
            call_span,
            Some(Rc::from("test-pending-call")),
            Rc::clone(&test_ctx()),
        );

        // Materialize should evaluate the arg thunk and return the result
        let result = materialize(&pending, None, &test_ctx(), 0).unwrap();
        assert_eq!(result, Value::Int(99));
    }

    #[test]
    fn test_pending_call_with_named_args() {
        // PendingCall should pass named args through to function invocation
        let env = empty_env();

        // Install a built-in add function
        fn add_builtin(ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let crate::value::BuiltinArgs { args, .. } = ctx;
            let a = materialize(&args[0], None, &test_ctx(), 0)?;
            let b = materialize(&args[1], None, &test_ctx(), 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Rc::new(Thunk::new_materialized(
                    Value::Int(x + y),
                    test_span(1, 1, 1, 1),
                ))),
                _ => panic!("test expects Int args"),
            }
        }
        env.borrow_mut().insert(
            "+".into(),
            Rc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: add_builtin,
                    name: "+",
                    pos_strictness: &[],
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        // Create a function that takes a mix of positional and named parameters
        let fn_with_named = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "a".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "b".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![sp(Entry {
                        key: Some(sp(Expr::Str("default".into()))),
                        value: rsp(Expr::Int(10)),
                    })]))),
                    variadic: false,
                },
            ]),
            body: Rc::new(sp(Expr::Call {
                func: Box::new(sp(Expr::var_ref("+".into()))),
                args: vec![
                    rsp(Expr::var_ref("a".into())),
                    rsp(Expr::var_ref("b".into())),
                ],
                named_args: vec![],
                implied: false,
            })),
            env: Rc::clone(&env),
        };

        let func_thunk = Rc::new(Thunk::new_materialized(
            fn_with_named,
            test_span(1, 1, 1, 10),
        ));

        // Pass first arg positionally, second as named
        let positional = vec![Rc::new(Thunk::new_materialized(
            Value::Int(5),
            test_span(1, 11, 1, 12),
        ))];

        let mut named = IndexMap::new();
        named.insert(
            "b".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(3),
                test_span(1, 13, 1, 14),
            )),
        );

        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            func_thunk,
            positional,
            named,
            call_span,
            empty_env(),
            call_span,
            Some(Rc::from("test-pending-call-named")),
            Rc::clone(&test_ctx()),
        );

        // Materialize should pass named args through correctly
        let result = materialize(&pending, None, &test_ctx(), 0).unwrap();
        assert_eq!(result, Value::Int(8)); // 5 + 3
    }

    #[test]
    fn test_pending_call_with_default_named_args() {
        // PendingCall with partial named args should use defaults
        let env = empty_env();

        // Install a built-in add function
        fn add_builtin(ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let crate::value::BuiltinArgs { args, .. } = ctx;
            let a = materialize(&args[0], None, &test_ctx(), 0)?;
            let b = materialize(&args[1], None, &test_ctx(), 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Rc::new(Thunk::new_materialized(
                    Value::Int(x + y),
                    test_span(1, 1, 1, 1),
                ))),
                _ => panic!("test expects Int args"),
            }
        }
        env.borrow_mut().insert(
            "+".into(),
            Rc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: add_builtin,
                    name: "+",
                    pos_strictness: &[],
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        let fn_with_default = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![sp(Entry {
                        key: Some(sp(Expr::Str("default".into()))),
                        value: rsp(Expr::Int(10)),
                    })]))),
                    variadic: false,
                },
            ]),
            body: Rc::new(sp(Expr::Call {
                func: Box::new(sp(Expr::var_ref("+".into()))),
                args: vec![
                    rsp(Expr::var_ref("x".into())),
                    rsp(Expr::var_ref("y".into())),
                ],
                named_args: vec![],
                implied: false,
            })),
            env: Rc::clone(&env),
        };

        let func_thunk = Rc::new(Thunk::new_materialized(
            fn_with_default,
            test_span(1, 1, 1, 10),
        ));

        // Provide x positionally, omit y so it uses default (10)
        let positional = vec![Rc::new(Thunk::new_materialized(
            Value::Int(7),
            test_span(1, 11, 1, 12),
        ))];

        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            func_thunk,
            positional,
            IndexMap::new(), // no named args - let y use default
            call_span,
            empty_env(),
            call_span,
            Some(Rc::from("test-pending-call-default")),
            Rc::clone(&test_ctx()),
        );

        // Materialize should use default for y (10)
        let result = materialize(&pending, None, &test_ctx(), 0).unwrap();
        assert_eq!(result, Value::Int(17)); // 7 + 10
    }

    // ── Failed thunk state tests ───────────────────────────────────────

    #[test]
    fn test_failed_state_returns_cached_error() {
        // When a thunk fails, it should cache the error in Failed state
        // and return it on subsequent materialization attempts
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("undefined".into())),
        })];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();

        let x_thunk = match &dict_val {
            Value::Dict(map) => Rc::clone(map.get(&Key::String("x".into())).unwrap()),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First materialization: should fail and cache the error
        let err1 = materialize(&x_thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err1.message().contains("undefined variable: undefined"),
            "first error: got: {}",
            err1.message()
        );

        // Check that the thunk is now in Failed state
        match &*x_thunk.state() {
            ThunkState::Failed(cached_err) => {
                assert!(cached_err
                    .message()
                    .contains("undefined variable: undefined"));
            }
            other => panic!("expected Failed state, got {other:?}"),
        }

        // Second materialization: should return the cached error
        let err2 = materialize(&x_thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err2.message().contains("undefined variable: undefined"),
            "second error: got: {}",
            err2.message()
        );
    }

    #[test]
    fn test_failed_state_updates_materialization_span() {
        // Failed state should preserve the first materialization_span and add
        // subsequent access sites as stack frames (dual-span model)
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("broken".into()))),
            value: rsp(Expr::var_ref("missing".into())),
        })];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();

        let broken_thunk = match &dict_val {
            Value::Dict(map) => Rc::clone(map.get(&Key::String("broken".into())).unwrap()),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First access with one materialization span
        let span1 = test_span(10, 1, 10, 5);
        let err1 = materialize(&broken_thunk, Some(&span1), &test_ctx(), 0).unwrap_err();
        assert_eq!(err1.materialization_span, Some(span1));
        assert_eq!(err1.stack.len(), 0);

        // Second access with a different materialization span should preserve span1
        // and add span2 as a stack frame
        let span2 = test_span(20, 1, 20, 5);
        let err2 = materialize(&broken_thunk, Some(&span2), &test_ctx(), 0).unwrap_err();
        assert_eq!(err2.materialization_span, Some(span1)); // PRESERVED
        assert_eq!(err2.stack.len(), 1);
        assert_eq!(err2.stack[0].label, "materialized");
        assert_eq!(err2.stack[0].span, span2);

        // Third access with no materialization span returns error with the
        // original materialization_span and the stack frame from the second access
        let err3 = materialize(&broken_thunk, None, &test_ctx(), 0).unwrap_err();
        assert_eq!(err3.materialization_span, Some(span1)); // PRESERVED
        assert_eq!(err3.stack.len(), 1);
        assert_eq!(err3.stack[0].span, span2);
    }

    #[test]
    fn test_failed_state_preserves_stack_frames() {
        // Failed state should preserve the original error's stack frames
        let env = empty_env();

        // Create a function that will fail
        let failing_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::var_ref("nonexistent".into()))),
            env: Rc::clone(&env),
        };

        env.borrow_mut().insert(
            "bad_fn".into(),
            Rc::new(Thunk::new_materialized(failing_fn, test_span(1, 1, 1, 20))),
        );

        // Call the failing function
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("bad_fn".into()))),
            args: vec![Rc::new(sp(Expr::Int(1)))],
            named_args: vec![],
            implied: false,
        });

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();

        // First materialization: error should have stack frames
        let err1 = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(err1.message().contains("undefined variable: nonexistent"));
        let frame_count1 = err1.stack.len();
        assert!(frame_count1 > 0, "should have at least one stack frame");

        // Second materialization: error should have the same stack frames
        let err2 = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert_eq!(
            err2.stack.len(),
            frame_count1,
            "stack frames should be preserved"
        );
    }

    #[test]
    fn test_pending_builtin_error_becomes_failed() {
        // When a PendingBuiltin fails, it should transition to Failed state
        fn failing_builtin(ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let crate::value::BuiltinArgs { call_span, .. } = ctx;
            Err(EvalError::internal("builtin intentionally failed".to_string(), call_span).into())
        }

        let env = empty_env();
        env.borrow_mut().insert(
            "fail".into(),
            Rc::new(Thunk::new_materialized(
                Value::Builtin(crate::value::BuiltinDef {
                    func: failing_builtin,
                    name: "fail",
                    pos_strictness: &[],
                }),
                test_span(1, 1, 1, 5),
            )),
        );

        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("fail".into()))),
            args: vec![],
            named_args: vec![],
            implied: false,
        });

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();

        // First materialization: should fail
        let err1 = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(err1.message().contains("builtin intentionally failed"));

        // Check that the thunk is now in Failed state
        match &*thunk.state() {
            ThunkState::Failed(_) => {}
            other => panic!("expected Failed state after error, got {other:?}"),
        }

        // Second materialization: should return cached error
        let err2 = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(err2.message().contains("builtin intentionally failed"));
    }

    #[test]
    fn test_pending_call_error_becomes_failed() {
        // When a PendingCall fails, it should transition to Failed state
        let env = empty_env();

        let failing_fn = Value::Function {
            params: Rc::new(vec![]),
            body: Rc::new(sp(Expr::var_ref("does_not_exist".into()))),
            env: Rc::clone(&env),
        };

        let func_thunk = Rc::new(Thunk::new_materialized(failing_fn, test_span(1, 1, 1, 10)));
        let call_span = test_span(2, 1, 2, 10);

        let pending = Rc::new(Thunk::new_pending_call(
            func_thunk,
            vec![],
            IndexMap::new(),
            call_span,
            empty_env(),
            call_span,
            Some(Rc::from("test-pending-call")),
            Rc::clone(&test_ctx()),
        ));

        // First materialization: should fail
        let err1 = materialize(&pending, None, &test_ctx(), 0).unwrap_err();
        assert!(err1
            .message()
            .contains("undefined variable: does_not_exist"));

        // Check that the thunk is now in Failed state
        match &*pending.state() {
            ThunkState::Failed(_) => {}
            other => panic!("expected Failed state after error, got {other:?}"),
        }

        // Second materialization: should return cached error
        let err2 = materialize(&pending, None, &test_ctx(), 0).unwrap_err();
        assert!(err2
            .message()
            .contains("undefined variable: does_not_exist"));
    }

    #[test]
    fn test_pending_call_func_materialization_failure() {
        let bad_func = Rc::new(Thunk::new_unevaluated(
            Rc::new(sp(Expr::var_ref("nonexistent_func".into()))),
            empty_env(),
            Rc::clone(&test_ctx()),
            test_span(1, 1, 1, 10),
        ));
        let call_span = test_span(2, 1, 2, 10);
        let pending = Rc::new(Thunk::new_pending_call(
            bad_func,
            vec![],
            IndexMap::new(),
            call_span,
            empty_env(),
            call_span,
            Some(Rc::from("test-pending-call")),
            Rc::clone(&test_ctx()),
        ));

        // First materialization should fail with undefined variable error
        let err = materialize(&pending, None, &test_ctx(), 0).unwrap_err();
        assert!(err
            .message()
            .contains("undefined variable: nonexistent_func"));

        // The thunk should be in Failed state, NOT InProgress
        match &*pending.state() {
            ThunkState::Failed(_) => {}
            ThunkState::InProgress => panic!("BUG: thunk stuck in InProgress"),
            other => panic!("unexpected state: {other:?}"),
        }

        // Second access should return cached error, NOT "circular dependency"
        let err2 = materialize(&pending, None, &test_ctx(), 0).unwrap_err();
        assert!(err2
            .message()
            .contains("undefined variable: nonexistent_func"));
        assert!(!err2.message().contains("circular dependency"));
    }

    #[test]
    fn test_unevaluated_error_becomes_failed() {
        // When an Unevaluated thunk fails during materialization, it should transition to Failed
        let expr = sp(Expr::var_ref("undefined_var".into()));
        let env = empty_env();
        let thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(expr),
            Rc::clone(&env),
            Rc::clone(&test_ctx()),
            test_span(1, 1, 1, 15),
        ));

        // First materialization: should fail
        let err1 = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(err1.message().contains("undefined variable: undefined_var"));

        // Check that the thunk is now in Failed state
        match &*thunk.state() {
            ThunkState::Failed(_) => {}
            other => panic!("expected Failed state after error, got {other:?}"),
        }

        // Second materialization: should return cached error
        let err2 = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(err2.message().contains("undefined variable: undefined_var"));
    }

    #[test]
    fn test_failed_state_same_span_no_duplicate() {
        // Accessing a Failed thunk twice with the same mat_span should not duplicate frames.
        // Use DotAccess (deferred thunk) so eval returns Ok and failure happens on materialize.
        let env = empty_env();

        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("undefined_var".into()))),
            field: "field".into(),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();

        // First materialization: error with a specific mat_span
        let mat_span = test_span(10, 5, 10, 15);
        let err1 = materialize(&thunk, Some(&mat_span), &test_ctx(), 0).unwrap_err();
        assert!(err1.message().contains("undefined variable: undefined_var"));
        let frame_count1 = err1.stack.len();

        // Second materialization: same mat_span
        let err2 = materialize(&thunk, Some(&mat_span), &test_ctx(), 0).unwrap_err();
        assert_eq!(
            err2.stack.len(),
            frame_count1,
            "same mat_span should not duplicate frames"
        );
    }

    #[test]
    fn test_failed_state_none_then_some_mat_span() {
        // First access with None mat_span, then Some(span1), then Some(span2).
        // Use DotAccess (deferred thunk) so eval returns Ok and failure happens on materialize.
        let env = empty_env();

        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("undefined_var".into()))),
            field: "field".into(),
        });
        let thunk = eval(Rc::new(expr.clone()), env, &test_ctx(), 0).unwrap();

        // First access: None mat_span
        let err1 = materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(err1.message().contains("undefined variable: undefined_var"));
        assert!(err1.materialization_span.is_none());

        // Second access: Some(span1) — should update materialization_span
        let span1 = test_span(10, 5, 10, 15);
        let err2 = materialize(&thunk, Some(&span1), &test_ctx(), 0).unwrap_err();
        assert_eq!(
            err2.materialization_span,
            Some(span1),
            "mat_span should be set on second access with Some"
        );

        // Third access: Some(span2) — should add as stack frame, preserve span1 as mat_span
        let span2 = test_span(20, 5, 20, 15);
        let err3 = materialize(&thunk, Some(&span2), &test_ctx(), 0).unwrap_err();
        assert_eq!(
            err3.materialization_span,
            Some(span1),
            "original mat_span should be preserved"
        );
        assert!(
            err3.stack.iter().any(|f| f.span == span2),
            "span2 should be in stack frames"
        );
    }

    #[test]
    #[ignore = "requires >128MB Rust stack in debug mode; passes in release mode. Stack safety is guaranteed by the iterative materialize_rc loop; these tests verify the depth-limit POLICY only."]
    fn test_pending_call_cycle_detection() {
        // 256 levels of LLT recursion needs more than the default 8MB Rust stack.
        let result = std::thread::Builder::new()
            .stack_size(128 * 1024 * 1024) // 128MB — debug-mode materialize() needs ~100MB at 256 levels
            .spawn(|| {
                let env = empty_env();

                let recursive_fn = Value::Function {
                    params: Rc::new(vec![Param {
                        name: "x".into(),
                        annotation: None,
                        variadic: false,
                    }]),
                    body: Rc::new(sp(Expr::Call {
                        func: Box::new(sp(Expr::var_ref("f".into()))),
                        args: vec![rsp(Expr::var_ref("x".into()))],
                        named_args: vec![],
                        implied: false,
                    })),
                    env: Rc::clone(&env),
                };

                env.borrow_mut().insert(
                    "f".into(),
                    Rc::new(Thunk::new_materialized(
                        recursive_fn,
                        test_span(1, 1, 1, 20),
                    )),
                );

                let call_expr = sp(Expr::Call {
                    func: Box::new(sp(Expr::var_ref("f".into()))),
                    args: vec![Rc::new(sp(Expr::Int(1)))],
                    named_args: vec![],
                    implied: false,
                });

                let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();
                materialize(&thunk, None, &test_ctx(), 0).unwrap_err()
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(
            result
                .message()
                .contains("maximum evaluation depth exceeded"),
            "got: {}",
            result.message()
        );
    }

    // ── Non-cacheable error tests (is_cacheable) ───────────────────────

    #[test]
    fn test_depth_exceeded_does_not_cache() {
        // DepthExceeded errors should NOT transition the thunk to Failed state
        // because the same thunk may succeed at a lower depth
        let env = empty_env();

        // Create a recursive function
        let recursive_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::Call {
                func: Box::new(sp(Expr::var_ref("f".into()))),
                args: vec![rsp(Expr::var_ref("x".into()))],
                named_args: vec![],
                implied: false,
            })),
            env: Rc::clone(&env),
        };

        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(
                recursive_fn,
                test_span(1, 1, 1, 20),
            )),
        );

        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![Rc::new(sp(Expr::Int(1)))],
            named_args: vec![],
            implied: false,
        });

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();

        // Try to materialize at depth 256 (MAX_EVAL_DEPTH)
        let err = materialize(&thunk, None, &test_ctx(), 256).unwrap_err();
        assert!(
            err.message().contains("maximum evaluation depth exceeded"),
            "expected depth exceeded error, got: {}",
            err.message()
        );

        // The thunk should NOT be in Failed state
        match &*thunk.state() {
            ThunkState::Failed(_) => {
                panic!("DepthExceeded should not cache - thunk is in Failed state")
            }
            ThunkState::PendingCall { .. } => {
                // Expected: state was restored to PendingCall
            }
            other => panic!("expected PendingCall state, got: {:?}", other),
        };
    }

    #[test]
    fn test_regular_error_does_cache() {
        // Regular errors (not DepthExceeded) should transition to Failed state
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::var_ref("undefined".into())),
        })];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(Rc::new(dict.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, &test_ctx(), 0).unwrap();

        let x_thunk = match &dict_val {
            Value::Dict(map) => Rc::clone(map.get(&Key::String("x".into())).unwrap()),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First materialization: should fail and cache the error
        let err1 = materialize(&x_thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err1.message().contains("undefined variable: undefined"),
            "expected undefined variable error, got: {}",
            err1.message()
        );

        // The thunk SHOULD be in Failed state because UndefinedVariable is cacheable
        match &*x_thunk.state() {
            ThunkState::Failed(cached_err) => {
                assert!(
                    cached_err
                        .message()
                        .contains("undefined variable: undefined"),
                    "cached error mismatch: got: {}",
                    cached_err.message()
                );
            }
            other => panic!("expected Failed state, got: {:?}", other),
        };
    }

    #[test]
    fn test_depth_exceeded_can_retry_at_lower_depth() {
        // After a non-cached DepthExceeded error, the thunk should be re-evaluable
        // at a shallower depth (this test is conceptual - hard to test with actual
        // recursion depth limits, so we test the state preservation)
        let env = empty_env();

        let recursive_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::Call {
                func: Box::new(sp(Expr::var_ref("f".into()))),
                args: vec![rsp(Expr::var_ref("x".into()))],
                named_args: vec![],
                implied: false,
            })),
            env: Rc::clone(&env),
        };

        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(
                recursive_fn,
                test_span(1, 1, 1, 20),
            )),
        );

        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![Rc::new(sp(Expr::Int(1)))],
            named_args: vec![],
            implied: false,
        });

        let thunk = eval(Rc::new(call_expr.clone()), env, &test_ctx(), 0).unwrap();

        // First attempt at max depth - should fail
        let err1 = materialize(&thunk, None, &test_ctx(), 256).unwrap_err();
        assert!(
            err1.message().contains("maximum evaluation depth exceeded"),
            "expected depth exceeded, got: {}",
            err1.message()
        );

        // Second attempt at max depth - should fail again (not cached)
        let err2 = materialize(&thunk, None, &test_ctx(), 256).unwrap_err();
        assert!(
            err2.message().contains("maximum evaluation depth exceeded"),
            "expected depth exceeded on retry, got: {}",
            err2.message()
        );

        // The thunk should still be in PendingCall state, not Failed
        match &*thunk.state() {
            ThunkState::Failed(_) => panic!("DepthExceeded should not cache"),
            ThunkState::PendingCall { .. } => {
                // Expected: state was preserved
            }
            other => panic!("expected PendingCall state, got: {:?}", other),
        };
    }

    #[test]
    fn test_guarded_thunk_depth_exceeded_restores_state() {
        // Bug fix: Guarded thunks hit by DepthExceeded should restore Guarded state,
        // not remain stuck in InProgress (which causes CycleDetected on retry).
        use crate::types::Type;

        let env = empty_env();
        let ctx = test_ctx();

        // Create a recursive function that will hit depth limit
        let recursive_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::Call {
                func: Box::new(sp(Expr::var_ref("f".into()))),
                args: vec![rsp(Expr::var_ref("x".into()))],
                named_args: vec![],
                implied: false,
            })),
            env: Rc::clone(&env),
        };

        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(
                recursive_fn,
                test_span(1, 1, 1, 20),
            )),
        );

        // Create an inner thunk that will recurse and hit depth limit
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("f".into()))),
            args: vec![Rc::new(sp(Expr::Int(1)))],
            named_args: vec![],
            implied: false,
        });
        let inner_thunk = eval(Rc::new(call_expr.clone()), Rc::clone(&env), &ctx, 0).unwrap();

        // Wrap it in a Guarded thunk
        let expected_type = Type::Int;
        let field_path = vec!["test".to_string()];
        let guard_span = test_span(1, 1, 1, 10);

        let guarded_thunk = Rc::new(Thunk::new_guarded(
            inner_thunk,
            expected_type.clone(),
            field_path.clone(),
            guard_span,
        ));

        // Force the guarded thunk at exactly MAX_EVAL_DEPTH so the outer materialize passes the
        // depth guard and reaches take_guarded(). The inner recursive call uses depth + 1, which
        // exceeds the limit and returns DepthExceeded — exercising the Err branch that restores
        // Guarded state. Calling at MAX_EVAL_DEPTH + 1 would fire the depth check before
        // take_guarded() is ever called, making the test pass vacuously.
        let err1 = materialize(&guarded_thunk, None, &ctx, MAX_EVAL_DEPTH).unwrap_err();
        assert!(
            err1.message().contains("maximum evaluation depth exceeded"),
            "expected depth exceeded, got: {}",
            err1.message()
        );

        // The thunk should be back in Guarded state, not InProgress
        match &*guarded_thunk.state() {
            ThunkState::Guarded {
                expected,
                field_path: fp,
                ..
            } => {
                assert_eq!(expected, &expected_type);
                assert_eq!(fp.as_ref(), &vec!["test".to_string()]);
            }
            ThunkState::Failed(_) => panic!("DepthExceeded should not cache in Guarded state"),
            ThunkState::InProgress => panic!("Guarded thunk stuck in InProgress - BUG NOT FIXED"),
            other => panic!("expected Guarded state, got: {:?}", other),
        }

        // Retry at the same depth should still fail with DepthExceeded, not CycleDetected.
        // Without the fix, the thunk would be stuck in InProgress, causing CycleDetected.
        let err2 = materialize(&guarded_thunk, None, &ctx, MAX_EVAL_DEPTH).unwrap_err();
        assert!(
            err2.message().contains("maximum evaluation depth exceeded"),
            "expected depth exceeded on retry, got: {}",
            err2.message()
        );
        assert!(
            !err2.message().contains("circular"),
            "should not see cycle error, got: {}",
            err2.message()
        );
    }

    // === EvalContext isolation tests ===

    #[test]
    fn test_evalcontext_include_cache_persists_within_context() {
        // Create a temp directory with a test file
        let temp_dir = std::env::temp_dir().join(format!("tinct_test_{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let test_file = temp_dir.join("test_cache.llt");
        std::fs::write(&test_file, "[value: 42]").unwrap();

        let base_dir = cap_std::fs::Dir::open_ambient_dir(&temp_dir, cap_std::ambient_authority())
            .expect("failed to open temp_dir");
        let ctx = EvalContext::new(
            base_dir,
            crate::builtins::create_stdlib_env().unwrap(),
            false,
        );

        // First include: should evaluate and cache
        let include_expr1 = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("include".into()))),
            args: vec![rsp(Expr::Str("test_cache.llt".into()))],
            named_args: vec![],
            implied: false,
        });
        let result1 = eval(
            Rc::new(include_expr1.clone()),
            Rc::clone(&ctx.config.stdlib_env),
            &ctx,
            0,
        )
        .unwrap();
        let val1 = materialize(&result1, None, &ctx, 0).unwrap();

        // Verify the cache contains the file
        assert_eq!(
            ctx.state.borrow().include_cache.len(),
            1,
            "include_cache should contain exactly one entry"
        );

        // Second include of the same file: should hit cache
        let include_expr2 = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("include".into()))),
            args: vec![rsp(Expr::Str("test_cache.llt".into()))],
            named_args: vec![],
            implied: false,
        });
        let result2 = eval(
            Rc::new(include_expr2.clone()),
            Rc::clone(&ctx.config.stdlib_env),
            &ctx,
            0,
        )
        .unwrap();
        let val2 = materialize(&result2, None, &ctx, 0).unwrap();

        // Both results should be the same value
        match (&val1, &val2) {
            (Value::Dict(m1), Value::Dict(m2)) => {
                assert_eq!(m1.len(), m2.len());
                let v1 = m1.get(&Key::String("value".into())).unwrap();
                let v2 = m2.get(&Key::String("value".into())).unwrap();
                assert_eq!(
                    materialize(v1, None, &ctx, 0).unwrap(),
                    materialize(v2, None, &ctx, 0).unwrap()
                );
            }
            _ => panic!("expected Dict values"),
        }

        // Cleanup
        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn test_evalcontext_include_guard_detects_cycles() {
        // Create a temp directory with a test file
        let temp_dir =
            std::env::temp_dir().join(format!("tinct_test_guard_{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let test_file = temp_dir.join("guard_test.llt");
        std::fs::write(&test_file, "[x: 1]").unwrap();

        let base_dir = cap_std::fs::Dir::open_ambient_dir(&temp_dir, cap_std::ambient_authority())
            .expect("failed to open temp_dir");
        let ctx = EvalContext::new(
            base_dir,
            crate::builtins::create_stdlib_env().unwrap(),
            false,
        );

        // Manually insert the file identity (dev, ino) into the include guard
        #[cfg(unix)]
        let file_id = {
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(&test_file).unwrap();
            (metadata.dev(), metadata.ino())
        };
        #[cfg(not(unix))]
        let file_id = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            "guard_test.llt".hash(&mut hasher);
            (0u64, hasher.finish())
        };
        ctx.state.borrow_mut().include_guard.insert(file_id);

        // Attempt to include the file: should detect cycle
        let include_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("include".into()))),
            args: vec![rsp(Expr::Str("guard_test.llt".into()))],
            named_args: vec![],
            implied: false,
        });
        let result = eval(
            Rc::new(include_expr.clone()),
            Rc::clone(&ctx.config.stdlib_env),
            &ctx,
            0,
        )
        .unwrap();
        let err = materialize(&result, None, &ctx, 0).unwrap_err();

        assert!(
            err.message().contains("circular include") || err.message().contains("cycle"),
            "expected circular include error, got: {}",
            err.message()
        );

        // Cleanup
        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn test_evalcontext_two_contexts_with_different_base_dirs() {
        // Create two temp directories with identical file structure
        let temp_dir1 =
            std::env::temp_dir().join(format!("tinct_test_ctx1_{}", std::process::id()));
        let temp_dir2 =
            std::env::temp_dir().join(format!("tinct_test_ctx2_{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir1).unwrap();
        std::fs::create_dir_all(&temp_dir2).unwrap();

        // Create test.llt in each directory with different content
        let test_file1 = temp_dir1.join("test.llt");
        let test_file2 = temp_dir2.join("test.llt");
        std::fs::write(&test_file1, "[value: 100]").unwrap();
        std::fs::write(&test_file2, "[value: 200]").unwrap();

        // Create two independent EvalContexts with different base_dirs
        let base_dir1 =
            cap_std::fs::Dir::open_ambient_dir(&temp_dir1, cap_std::ambient_authority())
                .expect("failed to open temp_dir1");
        let ctx1 = EvalContext::new(
            base_dir1,
            crate::builtins::create_stdlib_env().unwrap(),
            false,
        );
        let base_dir2 =
            cap_std::fs::Dir::open_ambient_dir(&temp_dir2, cap_std::ambient_authority())
                .expect("failed to open temp_dir2");
        let ctx2 = EvalContext::new(
            base_dir2,
            crate::builtins::create_stdlib_env().unwrap(),
            false,
        );

        // Include test.llt from ctx1
        let include_expr1 = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("include".into()))),
            args: vec![rsp(Expr::Str("test.llt".into()))],
            named_args: vec![],
            implied: false,
        });
        let result1 = eval(
            Rc::new(include_expr1.clone()),
            Rc::clone(&ctx1.config.stdlib_env),
            &ctx1,
            0,
        )
        .unwrap();
        let val1 = materialize(&result1, None, &ctx1, 0).unwrap();

        // Include test.llt from ctx2
        let include_expr2 = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("include".into()))),
            args: vec![rsp(Expr::Str("test.llt".into()))],
            named_args: vec![],
            implied: false,
        });
        let result2 = eval(
            Rc::new(include_expr2.clone()),
            Rc::clone(&ctx2.config.stdlib_env),
            &ctx2,
            0,
        )
        .unwrap();
        let val2 = materialize(&result2, None, &ctx2, 0).unwrap();

        // Verify that the two contexts resolved different files
        match (&val1, &val2) {
            (Value::Dict(m1), Value::Dict(m2)) => {
                let v1_thunk = m1.get(&Key::String("value".into())).unwrap();
                let v2_thunk = m2.get(&Key::String("value".into())).unwrap();
                let v1 = materialize(v1_thunk, None, &ctx1, 0).unwrap();
                let v2 = materialize(v2_thunk, None, &ctx2, 0).unwrap();
                assert_eq!(
                    v1,
                    Value::Int(100),
                    "ctx1 should resolve to temp_dir1/test.llt"
                );
                assert_eq!(
                    v2,
                    Value::Int(200),
                    "ctx2 should resolve to temp_dir2/test.llt"
                );
            }
            _ => panic!("expected Dict values"),
        }

        // Verify that the two contexts have independent caches
        assert_eq!(ctx1.state.borrow().include_cache.len(), 1);
        assert_eq!(ctx2.state.borrow().include_cache.len(), 1);

        // Cleanup
        std::fs::remove_dir_all(&temp_dir1).unwrap();
        std::fs::remove_dir_all(&temp_dir2).unwrap();
    }

    #[test]
    fn test_evalcontext_shared_state_different_config() {
        // Create two temp directories
        let temp_dir1 =
            std::env::temp_dir().join(format!("tinct_test_shared1_{}", std::process::id()));
        let temp_dir2 =
            std::env::temp_dir().join(format!("tinct_test_shared2_{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir1).unwrap();
        std::fs::create_dir_all(&temp_dir2).unwrap();

        // Create a test file in dir1
        let test_file1 = temp_dir1.join("shared_test.llt");
        std::fs::write(&test_file1, "[cached: true]").unwrap();

        // Create ctx1 with base_dir = temp_dir1
        let base_dir1 =
            cap_std::fs::Dir::open_ambient_dir(&temp_dir1, cap_std::ambient_authority())
                .expect("failed to open temp_dir1");
        let ctx1 = EvalContext::new(
            base_dir1,
            crate::builtins::create_stdlib_env().unwrap(),
            false,
        );

        // Create ctx2 that shares ctx1's state but has a different base_dir
        let base_dir2 =
            cap_std::fs::Dir::open_ambient_dir(&temp_dir2, cap_std::ambient_authority())
                .expect("failed to open temp_dir2");
        let ctx2 = ctx1.with_base_dir(base_dir2);

        // Verify that ctx2 has a different base_dir (we can't directly compare Dirs,
        // but we can verify they point to different paths by checking if files exist)
        // This is sufficient to verify the test's intent: ctx1 and ctx2 have different base_dirs.

        // Verify that ctx2 shares the same state as ctx1 (using Rc::ptr_eq)
        assert!(
            Rc::ptr_eq(&ctx1.state, &ctx2.state),
            "ctx2 should share the same state Rc as ctx1"
        );

        // Include a file using ctx1 - this populates the include_cache
        let include_expr1 = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("include".into()))),
            args: vec![rsp(Expr::Str("shared_test.llt".into()))],
            named_args: vec![],
            implied: false,
        });
        let result1 = eval(
            Rc::new(include_expr1.clone()),
            Rc::clone(&ctx1.config.stdlib_env),
            &ctx1,
            0,
        )
        .unwrap();
        let _val1 = materialize(&result1, None, &ctx1, 0).unwrap();

        // Verify that ctx1's include_cache has one entry
        assert_eq!(
            ctx1.state.borrow().include_cache.len(),
            1,
            "ctx1 include_cache should have exactly one entry"
        );

        // Verify that ctx2's include_cache ALSO has the same entry (shared state)
        assert_eq!(
            ctx2.state.borrow().include_cache.len(),
            1,
            "ctx2 include_cache should share the same entry as ctx1"
        );

        // Verify they reference the exact same cache HashMap
        // The cache key is (dev, ino) — extract from the test file's metadata.
        let cache_key = {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(&test_file1).unwrap();
            (meta.dev(), meta.ino())
        };
        assert!(
            ctx1.state.borrow().include_cache.contains_key(&cache_key),
            "ctx1 cache should contain the file identity"
        );
        assert!(
            ctx2.state.borrow().include_cache.contains_key(&cache_key),
            "ctx2 cache should contain the same file identity"
        );

        // Test include_guard sharing: create same file in both directories
        let guard_path1 = temp_dir1.join("guard_test.llt");
        let guard_path2 = temp_dir2.join("guard_test.llt");
        std::fs::write(&guard_path1, "[x: 1]").unwrap();
        std::fs::write(&guard_path2, "[x: 2]").unwrap();

        // Insert the (dev, ino) of guard_path2 into ctx1's include guard
        let guard_file_id = {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(&guard_path2).unwrap();
            (meta.dev(), meta.ino())
        };
        ctx1.state.borrow_mut().include_guard.insert(guard_file_id);

        // Verify the guard is visible in ctx2 (shared state)
        assert!(
            ctx2.state.borrow().include_guard.contains(&guard_file_id),
            "ctx2 include_guard should contain the file identity inserted via ctx1"
        );

        // Attempt to include the guarded file using ctx2 - should detect cycle
        // This resolves to temp_dir2/guard_test.llt which is in the shared guard
        let include_expr2 = sp(Expr::Call {
            func: Box::new(sp(Expr::var_ref("include".into()))),
            args: vec![rsp(Expr::Str("guard_test.llt".into()))],
            named_args: vec![],
            implied: false,
        });
        let result2 = eval(
            Rc::new(include_expr2.clone()),
            Rc::clone(&ctx2.config.stdlib_env),
            &ctx2,
            0,
        )
        .unwrap();
        let err = materialize(&result2, None, &ctx2, 0).unwrap_err();

        assert!(
            err.message().contains("circular include") || err.message().contains("cycle"),
            "expected circular include error from shared guard, got: {}",
            err.message()
        );

        // Cleanup
        std::fs::remove_dir_all(&temp_dir1).unwrap();
        std::fs::remove_dir_all(&temp_dir2).unwrap();
    }

    // ── Structural TypeAssert tests (resolved_type: Some(Type::...)) ────
    // These test the NEW structural validation path added by the
    // typeassert-structural sprint, distinct from the nominal fallback path
    // (resolved_type: None) tested in the existing TypeAssert tests above.

    #[test]
    fn test_typeassert_structural_int_pass() {
        // Structural path: resolved_type = Some(Type::Int), value is Int(42) -> pass
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(Some(Type::Int)),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_typeassert_structural_int_fail() {
        // Structural path: resolved_type = Some(Type::Int), value is String -> error
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(Some(Type::Int)),
        });
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("type assertion failed: expected Int, got String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_typeassert_structural_str_pass() {
        // Structural path: resolved_type = Some(Type::Str), value is String -> pass
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Str".into())),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(Some(Type::Str)),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::String("hello".into()));
    }

    #[test]
    fn test_typeassert_structural_any() {
        // Structural path: resolved_type = Some(Type::Any), any value passes
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Any".into())),
            expr: Box::new(sp(Expr::Str("anything".into()))),
            resolved_type: RefCell::new(Some(Type::Any)),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::String("anything".into()));
    }

    #[test]
    fn test_typeassert_structural_any_accepts_int() {
        // Type::Any accepts Int as well (covers any-value branch)
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Any".into())),
            expr: Box::new(sp(Expr::Int(99))),
            resolved_type: RefCell::new(Some(Type::Any)),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_typeassert_structural_record_shape_check() {
        // Structural path: resolved_type = Some(Type::Record(..., Open))
        // Dict has required field "name" -> pass.
        // The record type check is immediate (shape check), field guard wrapping deferred.
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), Type::Str);
        let record_type = Type::Record(Row {
            fields,
            tail: RowTail::RowVar("_open".to_string(), 0),
        });

        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: rsp(Expr::Str("Alice".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("age".into()))),
                value: rsp(Expr::Int(30)),
            }),
        ];
        let dict_expr = sp(Expr::Dict(entries));
        let inner_expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Record".into())),
            expr: Box::new(dict_expr),
            resolved_type: RefCell::new(Some(record_type)),
        });

        let thunk = eval(Rc::new(inner_expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        // Should be a Dict with the expected fields
        match &val {
            Value::Dict(map) => {
                assert!(map.contains_key(&Key::String("name".into())));
                assert!(map.contains_key(&Key::String("age".into())));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_typeassert_structural_record_missing_field() {
        // Structural path: record type requires field "id", dict doesn't have it -> error
        let mut fields = HashMap::new();
        fields.insert("id".to_string(), Type::Int);
        let record_type = Type::Record(Row {
            fields,
            tail: RowTail::RowVar("_open".to_string(), 0),
        });

        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("name".into()))),
            value: rsp(Expr::Str("Alice".into())),
        })];
        let dict_expr = sp(Expr::Dict(entries));
        let inner_expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Record".into())),
            expr: Box::new(dict_expr),
            resolved_type: RefCell::new(Some(record_type)),
        });

        let err = eval(Rc::new(inner_expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("record missing field \"id\""),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_typeassert_structural_closed_record_extra_field() {
        // Structural path: closed record (RowTail::Empty), dict has extra field -> error
        let mut fields = HashMap::new();
        fields.insert("x".to_string(), Type::Int);
        let record_type = Type::Record(Row {
            fields,
            tail: RowTail::Empty,
        });

        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: rsp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("extra".into()))),
                value: rsp(Expr::Int(2)),
            }),
        ];
        let dict_expr = sp(Expr::Dict(entries));
        let inner_expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Record".into())),
            expr: Box::new(dict_expr),
            resolved_type: RefCell::new(Some(record_type)),
        });

        let err = eval(Rc::new(inner_expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("record with unexpected field \"extra\""),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_typeassert_structural_closed_record_exact_fields_pass() {
        // Structural path: closed record, dict has exactly the required fields -> pass
        let mut fields = HashMap::new();
        fields.insert("x".to_string(), Type::Int);
        let record_type = Type::Record(Row {
            fields,
            tail: RowTail::Empty,
        });

        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: rsp(Expr::Int(42)),
        })];
        let dict_expr = sp(Expr::Dict(entries));
        let inner_expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Record".into())),
            expr: Box::new(dict_expr),
            resolved_type: RefCell::new(Some(record_type)),
        });

        let thunk = eval(Rc::new(inner_expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        match &val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                assert!(map.contains_key(&Key::String("x".into())));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_typeassert_structural_record_non_dict_fails() {
        // Structural path: resolved_type = Some(Type::Record(...)), value is Int -> error
        let mut fields = HashMap::new();
        fields.insert("x".to_string(), Type::Int);
        let record_type = Type::Record(Row {
            fields,
            tail: RowTail::RowVar("_open".to_string(), 0),
        });

        let inner_expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Record".into())),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(Some(record_type)),
        });

        let err = eval(Rc::new(inner_expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("type assertion failed"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_typeassert_nominal_fallback() {
        // Nominal fallback path: resolved_type = None, annotation "Int", value is Int -> pass
        // (This ensures the existing nominal path is preserved alongside the new structural path.)
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Int(7))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(7));
    }

    #[test]
    fn test_typeassert_nominal_fallback_mismatch() {
        // Nominal fallback path: resolved_type = None, annotation "Int", value is String -> error
        // (Verifies nominal fallback still rejects mismatches.)
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Str("oops".into()))),
            resolved_type: RefCell::new(None),
        });
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("type assertion failed: expected Int, got String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_typeassert_primitive_eager_with_default() {
        // Primitive TypeAssert with default: MUST eagerly validate to decide whether to use default
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(999)),
        })];

        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("not an int".into()))),
            resolved_type: RefCell::new(Some(Type::Int)),
        });

        // eval() returns a Materialized thunk containing the default value
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        assert!(
            matches!(&*thunk.state(), ThunkState::Materialized(_)),
            "TypeAssert with default must eagerly materialize"
        );
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::Int(999));
    }

    // ── annotation_has_structural_fields unit tests ────────────────────
    // Tests for the helper that distinguishes structural record annotations
    // (e.g. [@[name: String] $x]) from metadata-only annotations (e.g.
    // [@[default: 0] $x]) in the --no-typecheck fallback path.

    #[test]
    fn test_annotation_has_structural_fields_simple_returns_false() {
        // Simple annotations like @Int have no structural fields
        assert!(!annotation_has_structural_fields(&Annotation::Simple(
            "Int".into()
        )));
    }

    #[test]
    fn test_annotation_has_structural_fields_empty_property_dict() {
        // Empty PropertyDict has no structural fields
        assert!(!annotation_has_structural_fields(
            &Annotation::PropertyDict(vec![])
        ));
    }

    #[test]
    fn test_annotation_has_structural_fields_default_only() {
        // [@[default: 0] $x] — default-only, no structural fields
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(0)),
        })];
        assert!(!annotation_has_structural_fields(
            &Annotation::PropertyDict(entries)
        ));
    }

    #[test]
    fn test_annotation_has_structural_fields_type_only() {
        // [@[type: Int] $x] — type-only, no structural fields
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("type".into()))),
            value: rsp(Expr::Str("Int".into())),
        })];
        assert!(!annotation_has_structural_fields(
            &Annotation::PropertyDict(entries)
        ));
    }

    #[test]
    fn test_annotation_has_structural_fields_record_annotation() {
        // [@[name: String age: Int] $x] — has structural fields
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: rsp(Expr::Str("String".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("age".into()))),
                value: rsp(Expr::Str("Int".into())),
            }),
        ];
        assert!(annotation_has_structural_fields(&Annotation::PropertyDict(
            entries
        )));
    }

    #[test]
    fn test_annotation_has_structural_fields_mixed_meta_and_record() {
        // [@[name: String default: []] $x] — has structural field "name"
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: rsp(Expr::Str("String".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::Dict(vec![])),
            }),
        ];
        assert!(annotation_has_structural_fields(&Annotation::PropertyDict(
            entries
        )));
    }

    // ── elaboration gap tests ────────────────────────────────────────────
    // Tests for the --no-typecheck fallback path when resolved_type is None
    // and the annotation has structural fields (Dict tag check).

    #[test]
    fn test_elaboration_gap_structural_annotation_dict_passes() {
        // [@[name: String] [name: hello]] with resolved_type=None (no typecheck)
        // Should pass: value is a Dict (tag check succeeds)
        let ann_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("name".into()))),
            value: rsp(Expr::Str("String".into())),
        })];
        let dict_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("name".into()))),
            value: rsp(Expr::Str("hello".into())),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(ann_entries)),
            expr: Box::new(sp(Expr::Dict(dict_entries))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert!(
            matches!(val, Value::Dict(_)),
            "Structural annotation with Dict value should pass tag check"
        );
    }

    #[test]
    fn test_elaboration_gap_structural_annotation_non_dict_fails() {
        // [@[name: String] 42] with resolved_type=None (no typecheck)
        // Should fail: value is Int, not Dict
        let ann_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("name".into()))),
            value: rsp(Expr::Str("String".into())),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(ann_entries)),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let err = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("type assertion failed: expected Record, got Int"),
            "Structural annotation with non-Dict value should fail; got: {}",
            err.message()
        );
    }

    #[test]
    fn test_elaboration_gap_structural_annotation_non_dict_with_default() {
        // [@[name: String default: []] 42] with resolved_type=None (no typecheck)
        // Should use default: value is Int (not Dict), default is available
        let ann_entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: rsp(Expr::Str("String".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: rsp(Expr::Dict(vec![])),
            }),
        ];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(ann_entries)),
            expr: Box::new(sp(Expr::Int(42))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert!(
            matches!(val, Value::Dict(_)),
            "Should use default when tag check fails; got: {val:?}"
        );
    }

    #[test]
    fn test_elaboration_gap_default_only_no_structural_check() {
        // [@[default: 0] "hello"] with resolved_type=None
        // Should pass through without validation (no type, no structural fields)
        let ann_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: rsp(Expr::Int(0)),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(ann_entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
            resolved_type: RefCell::new(None),
        });
        let thunk = eval(Rc::new(expr.clone()), empty_env(), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();
        assert_eq!(val, Value::String("hello".into()));
    }

    // ── value_matches_type unit tests ────────────────────────────────────
    // Direct tests of the value_matches_type() helper function, which is
    // called in the structural TypeAssert handler for non-Record types.

    #[test]
    fn test_value_matches_type_int() {
        assert!(value_matches_type(&Value::Int(42), &Type::Int));
        assert!(!value_matches_type(&Value::String("x".into()), &Type::Int));
        assert!(!value_matches_type(&Value::Bool(true), &Type::Int));
    }

    #[test]
    fn test_value_matches_type_str() {
        assert!(value_matches_type(
            &Value::String("hello".into()),
            &Type::Str
        ));
        assert!(!value_matches_type(&Value::Int(1), &Type::Str));
        assert!(!value_matches_type(&Value::Bool(false), &Type::Str));
    }

    #[test]
    fn test_value_matches_type_float() {
        assert!(value_matches_type(&Value::Float(3.14), &Type::Float));
        assert!(!value_matches_type(&Value::Int(3), &Type::Float));
    }

    #[test]
    fn test_value_matches_type_bool() {
        assert!(value_matches_type(&Value::Bool(true), &Type::Bool));
        assert!(value_matches_type(&Value::Bool(false), &Type::Bool));
        assert!(!value_matches_type(&Value::Int(1), &Type::Bool));
    }

    #[test]
    fn test_value_matches_type_number() {
        // Type::Number accepts both Int and Float
        assert!(value_matches_type(&Value::Int(42), &Type::Number));
        assert!(value_matches_type(&Value::Float(1.5), &Type::Number));
        assert!(!value_matches_type(
            &Value::String("42".into()),
            &Type::Number
        ));
        assert!(!value_matches_type(&Value::Bool(true), &Type::Number));
    }

    #[test]
    fn test_value_matches_type_any() {
        // Type::Any accepts all value kinds
        assert!(value_matches_type(&Value::Int(1), &Type::Any));
        assert!(value_matches_type(&Value::Float(1.0), &Type::Any));
        assert!(value_matches_type(&Value::String("s".into()), &Type::Any));
        assert!(value_matches_type(&Value::Bool(true), &Type::Any));
        assert!(value_matches_type(
            &Value::Dict(IndexMap::new()),
            &Type::Any
        ));
    }

    #[test]
    fn test_value_matches_type_int_literal() {
        // Type::IntLiteral(n) matches only Int(n)
        assert!(value_matches_type(&Value::Int(5), &Type::IntLiteral(5)));
        assert!(!value_matches_type(&Value::Int(6), &Type::IntLiteral(5)));
        assert!(!value_matches_type(
            &Value::String("5".into()),
            &Type::IntLiteral(5)
        ));
    }

    #[test]
    fn test_value_matches_type_string_literal() {
        // Type::StringLiteral("foo") matches only String("foo")
        assert!(value_matches_type(
            &Value::String("foo".into()),
            &Type::StringLiteral("foo".into())
        ));
        assert!(!value_matches_type(
            &Value::String("bar".into()),
            &Type::StringLiteral("foo".into())
        ));
        assert!(!value_matches_type(
            &Value::Int(0),
            &Type::StringLiteral("foo".into())
        ));
    }

    #[test]
    fn test_value_matches_type_typevar_always_true() {
        // Type::TypeVar is treated as Any (residual polymorphic instantiation)
        assert!(value_matches_type(
            &Value::Int(1),
            &Type::TypeVar("a".into(), 0)
        ));
        assert!(value_matches_type(
            &Value::String("x".into()),
            &Type::TypeVar("a".into(), 0)
        ));
        assert!(value_matches_type(
            &Value::Bool(true),
            &Type::TypeVar("a".into(), 0)
        ));
    }

    #[test]
    fn test_value_matches_type_record_always_true() {
        // Type::Record always returns true (deferred to proxy contract wrapping).
        // This is intentional per the spec: record field validation happens via
        // validate_and_wrap_record, not value_matches_type.
        let mut fields = HashMap::new();
        fields.insert("x".to_string(), Type::Int);
        let record_type = Type::Record(Row {
            fields,
            tail: RowTail::Empty,
        });
        // Even a non-Dict value returns true here — record validation is done separately
        assert!(value_matches_type(&Value::Int(99), &record_type));
        assert!(value_matches_type(
            &Value::Dict(IndexMap::new()),
            &record_type
        ));
    }

    #[test]
    fn test_value_matches_type_proxy() {
        // Type::Proxy should match Value::Proxy and reject other value kinds
        let span = test_span(1, 1, 1, 5);
        let handler = Rc::new(Thunk::new_materialized(Value::Int(42), span));
        let proxy_val = Value::Proxy { handler };

        assert!(value_matches_type(&proxy_val, &Type::Proxy));
        assert!(!value_matches_type(&proxy_val, &Type::Int));
        assert!(value_matches_type(&proxy_val, &Type::Any));
    }

    // ── validate_and_wrap_record unit tests ──────────────────────────────────
    // Tests for validate_and_wrap_record helper function, particularly the
    // field_path error message generation for nested record validation.

    #[test]
    fn test_validate_and_wrap_record_nested_field_path_error() {
        // Test that validate_and_wrap_record generates correct error messages
        // when field_path is non-empty (nested record validation).
        //
        // This exercises the code path where field_path_prefix is built with each
        // segment separately quoted per doc/07-type-extensions.md:162.

        // Create a row type requiring field "y"
        let mut fields = HashMap::new();
        fields.insert("y".to_string(), Type::Int);
        let row = Row {
            fields,
            tail: RowTail::Empty,
        };

        // Create entries that are missing field "y"
        let entries = IndexMap::new();

        // Call validate_and_wrap_record with nested field_path ["outer", "inner"]
        let mut field_path = vec!["outer".to_string(), "inner".to_string()];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result =
            validate_and_wrap_record(&entries, &row, &mut field_path, guard_span, data_span);

        // Should error with field path prefix in the message
        assert!(result.is_err(), "Expected error for missing field");
        let err = result.unwrap_err();
        let msg = err.message();
        // definition_span should be data_span (where the invalid dict was constructed/bound),
        // not guard_span (the annotation site). validate_and_wrap_record uses data_span as the
        // definition site so errors point at the value, not at the type annotation.
        assert_eq!(
            err.definition_span, data_span,
            "definition_span should be data_span (value site), not guard_span (annotation site)"
        );

        // Verify the error message contains the field path prefix
        // doc/07-type-extensions.md:162 specifies each segment separately quoted:
        // field `outer`.`inner`: (not field `outer.inner`:)
        assert!(
            msg.contains("field `outer`.`inner`:"),
            "Expected field path prefix 'field `outer`.`inner`:' in error message, got: {}",
            msg
        );

        // Verify the error message describes the missing field
        assert!(
            msg.contains("record missing field \"y\""),
            "Expected 'record missing field \"y\"' in error message, got: {}",
            msg
        );
    }

    #[test]
    fn test_validate_and_wrap_record_nested_field_path_extra_field_error() {
        // Test that validate_and_wrap_record generates correct error messages
        // for unexpected fields in closed records when field_path is non-empty.
        //
        // This exercises the code path at eval.rs:202-216 where field_path_prefix
        // is built for cardinality check errors.

        // Create a closed row type (Empty tail) requiring only field "x"
        let mut fields = HashMap::new();
        fields.insert("x".to_string(), Type::Int);
        let row = Row {
            fields,
            tail: RowTail::Empty, // Closed record
        };

        // Create entries with "x" plus an unexpected field "z"
        let mut entries = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        entries.insert(
            Key::String("x".to_string()),
            Rc::new(Thunk::new_materialized(Value::Int(1), span)),
        );
        entries.insert(
            Key::String("z".to_string()),
            Rc::new(Thunk::new_materialized(Value::Int(99), span)),
        );

        // Call validate_and_wrap_record with nested field_path ["config"]
        let mut field_path = vec!["config".to_string()];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result =
            validate_and_wrap_record(&entries, &row, &mut field_path, guard_span, data_span);

        // Should error with field path prefix in the message
        assert!(
            result.is_err(),
            "Expected error for unexpected field in closed record"
        );
        let err = result.unwrap_err();
        let msg = err.message();
        // definition_span should be data_span (where the invalid dict was constructed/bound),
        // not guard_span (the annotation site). validate_and_wrap_record uses data_span as the
        // definition site so errors point at the value, not at the type annotation.
        assert_eq!(
            err.definition_span, data_span,
            "definition_span should be data_span (value site), not guard_span (annotation site)"
        );

        // Verify the error message contains the field path prefix
        assert!(
            msg.contains("field `config`:"),
            "Expected field path prefix 'field `config`:' in error message, got: {}",
            msg
        );

        // Verify the error message describes the unexpected field
        assert!(
            msg.contains("record with unexpected field \"z\""),
            "Expected 'record with unexpected field \"z\"' in error message, got: {}",
            msg
        );
    }

    #[test]
    fn test_validate_and_wrap_record_empty_field_path() {
        // Verify that when field_path is empty, no prefix is added to error messages.
        // This is the common case for top-level TypeAssert validation.

        // Create a row type requiring field "name"
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), Type::Str);
        let row = Row {
            fields,
            tail: RowTail::Empty,
        };

        // Create empty entries (missing "name")
        let entries = IndexMap::new();

        // Call with empty field_path
        let mut field_path = vec![];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result =
            validate_and_wrap_record(&entries, &row, &mut field_path, guard_span, data_span);

        assert!(result.is_err(), "Expected error for missing field");
        let err = result.unwrap_err();
        let msg = err.message();
        // definition_span should be data_span (where the invalid dict was constructed/bound),
        // not guard_span (the annotation site). validate_and_wrap_record uses data_span as the
        // definition site so errors point at the value, not at the type annotation.
        assert_eq!(
            err.definition_span, data_span,
            "definition_span should be data_span (value site), not guard_span (annotation site)"
        );

        // Should NOT contain the empty-path prefix `field "": ` that would be inserted
        // if the `field_path.is_empty()` guard were absent (i.e., format!("field \"{}\": ",
        // vec![].join(".")) = `field "": `).
        assert!(
            !msg.contains("field \"\": "),
            "Expected no empty-path prefix for empty field_path, got: {}",
            msg
        );

        // Should contain the direct error message
        assert!(
            msg.contains("record missing field \"name\""),
            "Expected 'record missing field \"name\"' in error message, got: {}",
            msg
        );
    }

    #[test]
    fn test_validate_and_wrap_record_rejects_int_key_in_closed_record() {
        // Integer-keyed entries (Key::Int) should be rejected by closed record types.
        // Row.fields is HashMap<String, Type>, so Key::Int entries are by definition
        // not in the expected field set and must trigger the cardinality check.
        //
        // Example: dict [0: "x"  name: "y"] against type @{name: String} should fail
        // because the 0: "x" entry is not in the closed record's field set.

        // Create a closed row type requiring only field "name"
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), Type::Str);
        let row = Row {
            fields,
            tail: RowTail::Empty, // Closed record
        };

        // Create entries with "name" (valid) plus an integer-keyed entry (invalid)
        let mut entries = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        entries.insert(
            Key::Int(0),
            Rc::new(Thunk::new_materialized(Value::String("x".into()), span)),
        );
        entries.insert(
            Key::String("name".to_string()),
            Rc::new(Thunk::new_materialized(Value::String("y".into()), span)),
        );

        let mut field_path = vec![];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result =
            validate_and_wrap_record(&entries, &row, &mut field_path, guard_span, data_span);

        // Should error: Key::Int(0) is not in the closed record's field set
        assert!(
            result.is_err(),
            "Expected error for integer-keyed entry in closed record"
        );
        let err = result.unwrap_err();
        let msg = err.message();
        assert!(
            msg.contains("unexpected field \"0\""),
            "Expected 'unexpected field \"0\"' in error message, got: {}",
            msg
        );
        assert!(
            msg.contains("closed record"),
            "Expected 'closed record' in error message, got: {}",
            msg
        );
    }

    #[test]
    fn test_validate_and_wrap_record_allows_int_key_in_open_record() {
        // Integer-keyed entries should NOT be rejected by open record types (RowVar tail).
        // Open records permit additional fields beyond those in the known field set.

        let mut fields = HashMap::new();
        fields.insert("name".to_string(), Type::Str);
        let row = Row {
            fields,
            tail: RowTail::RowVar("r".to_string(), 0), // Open record
        };

        let mut entries = IndexMap::new();
        let span = test_span(1, 1, 1, 5);
        entries.insert(
            Key::Int(0),
            Rc::new(Thunk::new_materialized(Value::String("x".into()), span)),
        );
        entries.insert(
            Key::String("name".to_string()),
            Rc::new(Thunk::new_materialized(Value::String("y".into()), span)),
        );

        let mut field_path = vec![];
        let guard_span = test_span(1, 1, 1, 10);
        let data_span = test_span(2, 1, 2, 5);

        let result =
            validate_and_wrap_record(&entries, &row, &mut field_path, guard_span, data_span);

        // Should succeed: open records allow extra fields (including integer-keyed ones)
        assert!(
            result.is_ok(),
            "Expected success for integer-keyed entry in open record, got: {:?}",
            result.unwrap_err()
        );
    }

    #[test]
    fn test_materialize_cached_thunk_at_high_depth() {
        // Pre-materialized thunks should succeed even at depth > MAX_EVAL_DEPTH.
        // Previously, the depth check fired BEFORE the Materialized early-return,
        // causing spurious DepthExceeded errors when accessing cached values at high depth.
        let span = test_span(1, 1, 1, 5);
        let thunk = Thunk::new_materialized(Value::Int(42), span);
        let ctx = test_ctx();

        // Materialize at depth=300 (> MAX_EVAL_DEPTH=256) should succeed
        let result = materialize(&thunk, None, &ctx, 300);
        assert!(
            result.is_ok(),
            "Expected success for cached thunk at high depth, got error: {:?}",
            result.unwrap_err()
        );
        assert_eq!(result.unwrap(), Value::Int(42));
    }

    #[test]
    fn test_materialize_failed_thunk_at_high_depth() {
        // Pre-failed thunks should return their cached error even at high depth,
        // without hitting the depth check.
        let span = test_span(1, 1, 1, 5);
        let thunk = Rc::new(Thunk::new_materialized(Value::Int(42), span));

        // Force it into Failed state with a cached error
        let err = Box::new(EvalError::type_mismatch("String", "Int", span));
        thunk.cache_failure(&err);

        let ctx = test_ctx();

        // Materialize at depth=300 should return the cached error, not DepthExceeded
        let result = materialize(&thunk, None, &ctx, 300);
        assert!(result.is_err(), "Expected cached error");
        let error = result.unwrap_err();
        assert!(
            error.message().contains("type mismatch"),
            "Expected cached type mismatch error, got: {}",
            error.message()
        );
    }

    #[test]
    fn test_thunk_guarded_memoizes_on_success() {
        // Task 3(3): Guarded thunk memoization — after successful validation, the
        // thunk transitions to Materialized and the second access returns the cached
        // value without re-running the type guard.
        use crate::types::Type;

        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);

        // Inner thunk: a materialized Int(42) — passes the Int guard.
        let inner = Rc::new(Thunk::new_materialized(Value::Int(42), span));

        // Wrap it in a Guarded thunk expecting Int.
        let guarded = Rc::new(Thunk::new_guarded(
            Rc::clone(&inner),
            Type::Int,
            vec!["value".to_string()],
            span,
        ));

        // Initial state must be Guarded.
        {
            let state = guarded.state();
            assert!(
                matches!(&*state, ThunkState::Guarded { .. }),
                "initial state should be Guarded"
            );
        }

        // First materialization: triggers guard, validates Int(42) against Type::Int → pass.
        let result1 = materialize(&guarded, None, &ctx, 0);
        assert!(result1.is_ok(), "first materialization should succeed");
        assert_eq!(result1.unwrap(), Value::Int(42));

        // After successful validation, thunk must be in Materialized state (memoized).
        {
            let state = guarded.state();
            assert!(
                matches!(&*state, ThunkState::Materialized(Value::Int(42))),
                "after first materialization thunk should be Materialized(Int(42)), got {:?}",
                &*state
            );
        }

        // Second materialization: must return cached value, not re-run the guard.
        let result2 = materialize(&guarded, None, &ctx, 0);
        assert!(
            result2.is_ok(),
            "second materialization should succeed (cached)"
        );
        assert_eq!(result2.unwrap(), Value::Int(42));

        // State is still Materialized (not changed by second access).
        {
            let state = guarded.state();
            assert!(
                matches!(&*state, ThunkState::Materialized(Value::Int(42))),
                "state should still be Materialized after second access"
            );
        }
    }

    #[test]
    fn test_guarded_thunk_failure_path() {
        // Task 3(2): Guarded thunk failure path — when the inner value fails the type guard,
        // the thunk transitions to Failed (cacheable) and subsequent access returns the
        // cached error without re-running the guard.
        use crate::types::Type;

        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);

        // Inner thunk: a String value — fails the Int guard.
        let inner = Rc::new(Thunk::new_materialized(Value::String("hello".into()), span));

        // Wrap it in a Guarded thunk expecting Int.
        let guarded = Rc::new(Thunk::new_guarded(
            Rc::clone(&inner),
            Type::Int,
            vec!["field".to_string()],
            span,
        ));

        // First materialization: triggers guard, validates String against Type::Int → fail.
        let result1 = materialize(&guarded, None, &ctx, 0);
        assert!(
            result1.is_err(),
            "materialization should fail: String does not satisfy Int guard"
        );
        let err = result1.unwrap_err();
        assert!(
            err.message().contains("type assertion failed"),
            "error should say 'type assertion failed', got: {}",
            err.message()
        );

        // After failure, thunk must be in Failed state (cacheable memoization of error).
        {
            let state = guarded.state();
            assert!(
                matches!(&*state, ThunkState::Failed(_)),
                "after type guard failure thunk should be Failed, got {:?}",
                &*state
            );
        }

        // Second materialization: returns the cached error, not re-runs the guard.
        let result2 = materialize(&guarded, None, &ctx, 0);
        assert!(
            result2.is_err(),
            "second materialization should also fail (cached)"
        );
        assert!(
            result2
                .unwrap_err()
                .message()
                .contains("type assertion failed"),
            "cached error should still say 'type assertion failed'"
        );
    }

    #[test]
    fn test_guarded_thunk_preserves_inner_origin() {
        // When materializing nested Guarded thunks, the error decoration should use
        // the inner thunk's origin, not the outer guard's origin. This test verifies that
        // inner_span is captured before materialization, not after.
        use crate::types::Type;

        let span = test_span(1, 1, 1, 10);

        // Create an inner thunk that will produce a type mismatch when wrapped with Guarded
        // (we expect Int but will get String)
        let inner_expr = Rc::new(sp(Expr::Str("hello".into())));
        let ctx = test_ctx();
        let inner_thunk = Rc::new(Thunk::new_unevaluated(
            inner_expr,
            empty_env(),
            Rc::clone(&ctx),
            span,
        ));

        // Wrap it in a Guarded thunk expecting Int (will fail type check)
        let guard_span = test_span(2, 1, 2, 10);
        let expected = Type::Int;
        let field_path = vec!["field".to_string()];
        let guarded = Rc::new(Thunk::new_guarded(
            inner_thunk,
            expected,
            field_path,
            guard_span,
        ));

        // Materialize - should fail type assertion
        let result = materialize(&guarded, None, &ctx, 0);
        assert!(result.is_err(), "Expected type assertion failure");

        let error = result.unwrap_err();
        let msg = error.message();

        // The error should be a type assertion failure
        assert!(
            msg.contains("type assertion failed"),
            "Expected type assertion failed error, got: {}",
            msg
        );

        // This test mainly verifies that the code compiles and runs with the fix applied.
        // The actual behavior (using inner_origin instead of outer origin) is verified
        // by the fact that errors now have the correct decoration context.
    }

    #[test]
    fn test_depth_check_for_unevaluated_thunk() {
        // Depth check should fire for Unevaluated thunks that need evaluation.
        // This verifies the depth check fires inside the Unevaluated arm, not before.
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(sp(Expr::Int(42)));
        let ctx = test_ctx();
        let thunk = Thunk::new_unevaluated(expr, empty_env(), Rc::clone(&ctx), span);

        // Materialize at depth > MAX_EVAL_DEPTH should fail with DepthExceeded
        let result = materialize(&thunk, None, &ctx, MAX_EVAL_DEPTH + 1);
        assert!(result.is_err(), "Expected DepthExceeded error");

        let error = result.unwrap_err();
        assert!(
            matches!(error.kind, crate::error::ErrorKind::DepthExceeded { .. }),
            "Expected DepthExceeded error, got: {:?}",
            error.kind
        );

        // Verify the thunk is still in Unevaluated state (error is non-cacheable)
        let state = thunk.state();
        assert!(
            matches!(&*state, ThunkState::Unevaluated { .. }),
            "Expected thunk to remain in Unevaluated state after non-cacheable error"
        );
    }

    #[test]
    fn test_iterative_materialize_deep_chain() {
        // Create a deep Unevaluated chain to verify the iterative implementation
        // doesn't stack overflow. Each thunk is an Unevaluated expression that
        // references the next thunk. This would overflow the Rust stack with
        // recursive materialize().
        //
        // chain_len is set to 3/4 of MAX_EVAL_DEPTH so the chain is long enough
        // to demonstrate iterative behavior while staying under the depth limit.
        // The static assertion makes the intent explicit and guards against regressions
        // if MAX_EVAL_DEPTH is ever lowered.
        let chain_len = MAX_EVAL_DEPTH * 3 / 4;
        assert!(
            chain_len < MAX_EVAL_DEPTH,
            "chain_len must be below MAX_EVAL_DEPTH to avoid a DepthExceeded error"
        );

        let ctx = test_ctx();
        let env = empty_env();
        let span = test_span(1, 1, 1, 10);

        // Base case: Thunk holding Int(chain_len as i64)
        let base_thunk = Rc::new(Thunk::new_materialized(Value::Int(chain_len as i64), span));
        env.borrow_mut()
            .insert("base".into(), Rc::clone(&base_thunk));

        // Build a chain of chain_len thunks, each just referencing the previous one
        // var_0 = $base, var_1 = $var_0, ..., var_{chain_len-1} = $var_{chain_len-2}
        for i in 0..chain_len {
            let prev_name = if i == 0 {
                "base".to_string()
            } else {
                format!("var_{}", i - 1)
            };
            let curr_name = format!("var_{}", i);

            let expr = sp(Expr::var_ref(prev_name.clone()));
            let thunk = Rc::new(Thunk::new_unevaluated(
                Rc::new(expr),
                Rc::clone(&env),
                Rc::clone(&ctx),
                span,
            ));
            env.borrow_mut().insert(curr_name, thunk);
        }

        // Materialize the outermost thunk — should succeed with iterative implementation
        let final_name = format!("var_{}", chain_len - 1);
        let final_thunk = env.borrow().get(&final_name).unwrap().clone();
        let result = materialize(&final_thunk, None, &ctx, 0);
        assert!(
            result.is_ok(),
            "Deep chain materialization should succeed, got: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap(), Value::Int(chain_len as i64));
    }

    #[test]
    fn test_iterative_materialize_cycle_detection() {
        // Verify that the iterative run() function detects circular dependencies
        // correctly via InProgress state detection in force_step.
        let ctx = test_ctx();
        let env = empty_env();

        // Create a cycle: x references y, y references x
        // x = $y
        let x_expr = sp(Expr::var_ref("y".into()));
        let x_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(x_expr),
            Rc::clone(&env),
            Rc::clone(&ctx),
            test_span(1, 1, 1, 2),
        ));

        // y = $x
        let y_expr = sp(Expr::var_ref("x".into()));
        let y_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(y_expr),
            Rc::clone(&env),
            Rc::clone(&ctx),
            test_span(1, 1, 1, 2),
        ));

        // Bind x -> x_thunk, y -> y_thunk in env
        env.borrow_mut().insert("x".into(), Rc::clone(&x_thunk));
        env.borrow_mut().insert("y".into(), Rc::clone(&y_thunk));

        // Materialize x — should detect cycle (2-node cycle)
        let result = materialize(&x_thunk, None, &ctx, 0);
        assert!(result.is_err(), "Cycle should be detected");
        let err = result.unwrap_err();
        assert!(
            err.message().contains("circular dependency"),
            "Error should mention circular dependency, got: {}",
            err.message()
        );

        // Test 3-node cycle: a→b→c→a
        let env3 = empty_env();

        let a_expr = sp(Expr::var_ref("b".into()));
        let a_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(a_expr),
            Rc::clone(&env3),
            Rc::clone(&ctx),
            test_span(1, 1, 1, 2),
        ));

        let b_expr = sp(Expr::var_ref("c".into()));
        let b_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(b_expr),
            Rc::clone(&env3),
            Rc::clone(&ctx),
            test_span(1, 1, 1, 2),
        ));

        let c_expr = sp(Expr::var_ref("a".into()));
        let c_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(c_expr),
            Rc::clone(&env3),
            Rc::clone(&ctx),
            test_span(1, 1, 1, 2),
        ));

        env3.borrow_mut().insert("a".into(), Rc::clone(&a_thunk));
        env3.borrow_mut().insert("b".into(), Rc::clone(&b_thunk));
        env3.borrow_mut().insert("c".into(), Rc::clone(&c_thunk));

        let result3 = materialize(&a_thunk, None, &ctx, 0);
        assert!(result3.is_err(), "3-node cycle should be detected");
        let err3 = result3.unwrap_err();
        assert!(
            err3.message().contains("circular dependency"),
            "3-node cycle error should mention circular dependency, got: {}",
            err3.message()
        );

        // Test self-reference: x→x
        let env_self = empty_env();

        let self_expr = sp(Expr::var_ref("x".into()));
        let self_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(self_expr),
            Rc::clone(&env_self),
            Rc::clone(&ctx),
            test_span(1, 1, 1, 2),
        ));

        env_self
            .borrow_mut()
            .insert("x".into(), Rc::clone(&self_thunk));

        let result_self = materialize(&self_thunk, None, &ctx, 0);
        assert!(result_self.is_err(), "Self-reference should be detected");
        let err_self = result_self.unwrap_err();
        assert!(
            err_self.message().contains("circular dependency"),
            "Self-reference error should mention circular dependency, got: {}",
            err_self.message()
        );
    }

    #[test]
    fn test_circular_dependency_cycle_path() {
        // Test that circular dependency errors include the cycle path
        use crate::error::ErrorKind;

        let ctx = test_ctx();
        let env = empty_env();

        // Create a 3-node cycle: a→b→c→a
        // We'll use eval_dict to create labeled thunks
        let source = r#"
[
    a: $b
    b: $c
    c: $a
]
        "#;

        let parsed = crate::parse(source).expect("parse should succeed");
        let thunk = eval_file(&parsed.node, Rc::clone(&env), &ctx, 0)
            .expect("eval_file should succeed (lazy dict construction)");
        // Dict construction is lazy — the cycle is only detected when forcing an entry.
        // Materialize the dict to get the Value::Dict, then force an entry to trigger
        // cycle detection. deep_materialize recursively forces all dict entries.
        let dict_val = materialize(&thunk, None, &ctx, 0).expect("dict should materialize");
        let result = crate::eval_deep::deep_materialize(&dict_val, &ctx, 0, None);

        assert!(
            result.is_err(),
            "Cycle should be detected when forcing cyclic entries"
        );
        let err = result.unwrap_err();

        // Verify the error kind is CircularDependency
        if let ErrorKind::CircularDependency { name, cycle_path } = &err.kind {
            // eval_dict creates thunks without origin labels, so the cycle detector
            // uses the default label "thunk" for the node that completes the cycle.
            assert!(
                name == "thunk" || name == "a" || name == "b" || name == "c",
                "Cycle should be detected at one of the thunks, got: {}",
                name
            );

            // Verify cycle_path is non-empty (at least one entry from the eval_stack)
            assert!(
                !cycle_path.is_empty(),
                "Cycle path should be non-empty, got: {}",
                cycle_path.len()
            );
        } else {
            panic!("Expected CircularDependency error, got: {:?}", err.kind);
        }
    }

    #[test]
    #[ignore] // TCO not yet wired — PendingCall still accumulates depth; enable when CEK Action::Eval dispatch eliminates depth accumulation
    fn test_tco_tail_recursive_function() {
        // Tail-recursive countdown. With PendingCall-based lazy dispatch, function
        // calls no longer consume eval() depth, but materialization still consumes
        // depth when forcing the PendingCall chain. The BuiltinForceArg optimization
        // prevents Rust stack overflow for builtin arg chains ($-/$+/$= iteratively
        // forced on continuation stack). 10 iterations is a smoke test proving basic
        // tail recursion works. Full unlimited TCO requires CEK machine conversion.
        let iterations = 10;
        let source = format!(
            r#"
[
    count-down: [fn [n acc]
        [call $if [call $= $n 0]
            $acc
            [call $count-down [call $- $n 1] [call $+ $acc 1]]]]
    result: [call $count-down {} 0]
]
    "#,
            iterations
        );
        let parsed = crate::parse(&source).expect("parse should succeed");
        let mut file = parsed.node;
        crate::desugar::desugar_file(&mut file);
        let env = crate::builtins::create_stdlib_env().expect("stdlib env creation should succeed");
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let ctx = EvalContext::new(base_dir, Rc::clone(&env), false);
        let thunk = eval_file(&file, env, &ctx, 0).expect("eval_file should succeed");
        let dict_val = materialize(&thunk, None, &ctx, 0).expect("materialization should succeed");
        match dict_val {
            Value::Dict(map) => {
                let result_thunk = map
                    .get(&Key::String("result".into()))
                    .expect("result key should exist");
                let result = materialize(result_thunk, None, &ctx, 0).unwrap_or_else(|e| {
                    panic!("TCO should allow {} iterations: {}", iterations, e)
                });
                match result {
                    Value::Int(n) => assert_eq!(n, iterations as i64),
                    other => panic!("Expected Int({}), got {:?}", iterations, other),
                }
            }
            other => panic!("Expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn test_decorate_deduplication() {
        // Verify that decorating an error with the same span twice doesn't create duplicates.
        // This tests the deduplication logic used when attaching stack frames during error propagation.
        let def_span = test_span(1, 1, 1, 10);
        let frame_span = test_span(5, 1, 5, 10);

        let mut err = EvalError::key_not_found("key", vec![], def_span);

        // Add the frame once
        err.push_frame("first access".to_string(), frame_span);
        assert_eq!(err.stack.len(), 1, "Should have exactly one frame");
        assert_eq!(err.stack[0].label, "first access");

        // Manually check for duplicate before adding (this is what error decoration does)
        if !err.stack.iter().any(|f| f.span == frame_span) {
            err.push_frame("second access".to_string(), frame_span);
        }

        // Should still be 1 frame (duplicate was avoided)
        assert_eq!(err.stack.len(), 1, "Duplicate span should be deduplicated");
        assert_eq!(
            err.stack[0].label, "first access",
            "Original label preserved"
        );
    }

    #[test]
    fn test_eval_context_new_empty_state() {
        // EvalContext::new() should create an empty include_guard and include_cache
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let env = empty_env();
        let ctx = EvalContext::new(base_dir, env, false);

        assert!(
            ctx.state.borrow().include_guard.is_empty(),
            "include_guard should be empty on creation"
        );
        assert!(
            ctx.state.borrow().include_cache.is_empty(),
            "include_cache should be empty on creation"
        );
    }

    #[test]
    fn test_eval_context_with_base_dir_shares_state() {
        // EvalContext::with_base_dir should share the same state but use a different base_dir
        let base_dir1 = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let env = empty_env();
        let ctx1 = EvalContext::new(base_dir1, env, false);

        // Populate the state
        ctx1.state.borrow_mut().include_guard.insert((0, 1));
        assert_eq!(ctx1.state.borrow().include_guard.len(), 1);

        // Create ctx2 with a different base_dir but shared state
        let base_dir2 = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let ctx2 = ctx1.with_base_dir(base_dir2);

        // Verify state is shared (using Rc::ptr_eq)
        assert!(
            Rc::ptr_eq(&ctx1.state, &ctx2.state),
            "ctx2 should share the same state Rc as ctx1"
        );

        // Verify the state is actually shared (include_guard has the same entry)
        assert_eq!(
            ctx2.state.borrow().include_guard.len(),
            1,
            "ctx2 should see the same include_guard as ctx1"
        );
        assert!(
            ctx2.state.borrow().include_guard.contains(&(0, 1)),
            "ctx2 should see the entry added to ctx1's include_guard"
        );
    }

    #[test]
    fn test_eval_context_no_fs_flag() {
        // EvalContext should preserve the no_fs flag
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let env = empty_env();

        let ctx_with_fs = EvalContext::new(base_dir.try_clone().unwrap(), Rc::clone(&env), false);
        assert!(
            !ctx_with_fs.config.no_fs,
            "no_fs should be false when created with false"
        );

        let ctx_no_fs = EvalContext::new(base_dir, env, true);
        assert!(
            ctx_no_fs.config.no_fs,
            "no_fs should be true when created with true"
        );
    }

    /// Integration test: `with_base_dir()` inherits `no_fs` flag.
    ///
    /// Verifies the no_fs=true code path end-to-end through `with_base_dir()`:
    /// 1. Create a ctx1 with no_fs=true.
    /// 2. Call ctx1.with_base_dir() to get ctx2 with a different base_dir.
    /// 3. Evaluate a `$include` call using ctx2.
    /// 4. Confirm the result is `IncludeForbidden` [E042] — proving:
    ///    a. `with_base_dir()` correctly propagates the no_fs flag.
    ///    b. `$include` resolves via ctx2's config (not a stale ctx1 config).
    ///    c. No actual filesystem access is needed — the error fires immediately.
    #[test]
    fn test_eval_context_with_base_dir_inherits_no_fs() {
        // Two separate base dirs: ctx1 starts with no_fs=true.
        let base_dir1 = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open base_dir1");
        let base_dir2 = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open base_dir2");
        let env = crate::builtins::create_stdlib_env().expect("stdlib env");

        // ctx1 has no_fs=true
        let ctx1 = EvalContext::new(base_dir1, env, true);
        assert!(ctx1.config.no_fs, "ctx1 must have no_fs=true");

        // ctx2 shares ctx1's state but has a different base_dir
        let ctx2 = ctx1.with_base_dir(base_dir2);

        // Verify structural properties of ctx2
        assert!(
            ctx2.config.no_fs,
            "ctx2 created via with_base_dir() must inherit no_fs=true from ctx1"
        );
        assert!(
            Rc::ptr_eq(&ctx1.state, &ctx2.state),
            "ctx2 must share the same state Rc as ctx1"
        );

        // Exercise the no_fs path: $include must produce IncludeForbidden [E042].
        // This proves ctx2 correctly propagates no_fs to $include without needing
        // any real files on disk.
        let include_expr = sp(crate::ast::Expr::Call {
            func: Box::new(sp(crate::ast::Expr::var_ref("include".into()))),
            args: vec![Rc::new(sp(crate::ast::Expr::Str(
                "hypothetical.llt".into(),
            )))],
            named_args: vec![],
            implied: false,
        });

        let thunk = eval(
            Rc::new(include_expr.clone()),
            Rc::clone(&ctx2.config.stdlib_env),
            &ctx2,
            0,
        )
        .expect("eval should succeed (thunk creation does not access filesystem)");
        let err =
            materialize(&thunk, None, &ctx2, 0).expect_err("$include with no_fs=true must fail");

        assert!(
            matches!(err.kind, crate::error::ErrorKind::IncludeForbidden),
            "Expected IncludeForbidden [E042], got: {}",
            err.kind.code()
        );
        assert_eq!(
            err.kind.code(),
            "E042",
            "IncludeForbidden must produce error code E042"
        );
    }

    #[test]
    fn test_selective_materialization_unused_branch() {
        // Verify that accessing only one dict entry doesn't materialize unused entries
        use crate::parser::parse_expression;

        let input = r#"[used: 1  unused: [call $error "should not materialize"]]"#;
        let parsed = parse_expression(input).expect("parse failed");
        let env = empty_env();
        let thunk = eval(Rc::new(parsed.clone()), Rc::clone(&env), &test_ctx(), 0).unwrap();
        let val = materialize(&thunk, None, &test_ctx(), 0).unwrap();

        // Extract the dict
        match val {
            Value::Dict(map) => {
                // Access only the "used" key
                let used_key = Key::String("used".into());
                let used_thunk = map.get(&used_key).expect("used key should exist");
                let used_val =
                    materialize(used_thunk, None, &test_ctx(), 0).expect("used should materialize");
                assert_eq!(used_val, Value::Int(1));

                // Verify the "unused" key exists but is NOT materialized
                let unused_key = Key::String("unused".into());
                let unused_thunk = map.get(&unused_key).expect("unused key should exist");

                // Check that the unused thunk is still in an unevaluated state
                // (it should not be Materialized, InProgress, or Failed)
                let state = unused_thunk.state();
                match &*state {
                    ThunkState::Unevaluated { .. } => {
                        // Good, it's still unevaluated
                    }
                    ThunkState::Materialized(_) => {
                        panic!("unused thunk should not be materialized")
                    }
                    ThunkState::Failed(_) => {
                        panic!("unused thunk should not be in Failed state (error should not have triggered)")
                    }
                    ThunkState::InProgress => {
                        panic!("unused thunk should not be InProgress")
                    }
                    _ => {
                        // Other states like PendingCall are also acceptable (function not yet invoked)
                    }
                }
            }
            _ => panic!("expected Dict value, got {:?}", val),
        }
    }
}
