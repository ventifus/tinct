//! `core_builtins()` — aggregator for all "core" module Rust builtins (T-713).
//!
//! This module is the single source of truth for builtins in the "core" module.
//! Implementations live in the individual split files (`builtins_math.rs`,
//! `builtins_dict.rs`, `builtins_string.rs`, etc.); this file only registers them
//! with their `builtin-*` names and strictness annotations.
//!
//! `builtin_module("core")` in `src/builtins.rs` delegates to `core_builtins()`.
//! Datetime builtins are in `datetime_builtins()` (T-715 complete).
//! Net/URI builtins are in `net_builtins()` (T-716, T-719 complete).
//!
//! ## What belongs here
//!
//! All "core" builtins — arithmetic, comparison, strings, bytes, I/O, sequences,
//! async concurrency, meta/reflection, and decomposed include primitives — **except**
//! `reduce_dict_step` / `reduce_seq_step`. Those two are internal PendingBuiltin
//! continuation helpers invoked only via embedded Rust function pointers — never
//! looked up by name from tinct code — and are excluded from the name-based registry
//! per the whatif spec (§Module Contents).

use crate::builtins::builtin;
// Arithmetic, comparison, bitwise, type-conversion, and control-flow implementations.
use crate::builtins_math::{
    builtin_acos, builtin_add, builtin_asin, builtin_atan, builtin_atan2, builtin_band,
    builtin_bor, builtin_bxor, builtin_cos, builtin_div_float, builtin_eq_float, builtin_eq_int,
    builtin_eq_string, builtin_exp, builtin_finite_check, builtin_float, builtin_gt, builtin_gte,
    builtin_if, builtin_inf_check, builtin_log, builtin_log10, builtin_log2, builtin_lt,
    builtin_lte, builtin_mul, builtin_nan_check, builtin_pow, builtin_shl, builtin_shr,
    builtin_sin, builtin_sqrt, builtin_sub, builtin_tan,
};
// Dict/access implementations.
use crate::builtins_dict::{
    builtin_append, builtin_build_dict, builtin_builder_delete, builtin_builder_finish,
    builtin_builder_get, builtin_builder_get_or, builtin_builder_has, builtin_builder_set,
    builtin_builder_snapshot, builtin_dict_has_key_nth, builtin_dict_has_kv_nth,
    builtin_dict_has_nth, builtin_dict_key_nth, builtin_dict_kv_nth, builtin_dict_nth,
    builtin_field_get, builtin_get, builtin_get_by_field, builtin_has_key, builtin_keys,
    builtin_length, builtin_make_builder, builtin_merge, builtin_slot_get,
};
// String implementations.
use crate::builtins_string::{
    builtin_bytes_str, builtin_char_code, builtin_chr, builtin_float_to_string,
    builtin_int_to_string, builtin_regex_match, builtin_replace, builtin_str_byte_count,
    builtin_str_bytes, builtin_str_has_nth, builtin_str_has_nth_byte, builtin_str_index_of,
    builtin_str_length, builtin_str_map_chars, builtin_str_nth_byte, builtin_str_nth_char,
    builtin_str_slice, builtin_str_to_lower_char, builtin_str_to_upper_char, builtin_string_concat,
    builtin_trim, builtin_trim_end, builtin_trim_start,
};
// Bytes implementations.
use crate::builtins_bytes::{
    builtin_bytes, builtin_bytes_concat, builtin_bytes_equal, builtin_bytes_find,
    builtin_bytes_get, builtin_bytes_of, builtin_bytes_slice, builtin_ct_equal, builtin_encode,
};
// Numeric (floor, round) and parsing (to-int, to-float) implementations — live in builtins.rs.
use crate::builtins::{builtin_floor, builtin_round, builtin_to_float, builtin_to_int};
// Stream output implementations.
use crate::stream::builtin_to_tinct;
// Meta/eval implementations.
use crate::builtins_meta::{
    builtin_annotation_of, builtin_apply, builtin_ast_of, builtin_ast_to_program, builtin_big_int,
    builtin_blake3, builtin_builtin_module, builtin_cap_env_has, builtin_cap_identity,
    builtin_check_type, builtin_current_env, builtin_decimal, builtin_eval, builtin_eval_macro_ast,
    builtin_eval_repr, builtin_eval_types, builtin_extend_env, builtin_force,
    builtin_fork_type_ctx, builtin_gensym, builtin_get_type_context, builtin_include_cache_get,
    builtin_include_cache_put, builtin_is_contractive, builtin_llt_repr, builtin_load,
    builtin_macro_error, builtin_macro_injects, builtin_make_annotated, builtin_make_type_ctx,
    builtin_parse, builtin_program, builtin_raise, builtin_resolve, builtin_sequential,
    builtin_span_of, builtin_tag_of, builtin_try, builtin_type_of, builtin_typecheck,
    builtin_until, builtin_validate, builtin_var_resolution, builtin_variant_payload,
};
// I/O implementations.
use crate::builtins_dict::{builtin_concat, builtin_drop, builtin_take};
use crate::builtins_io::{
    // builtin_cap_data, builtin_close, builtin_flush, builtin_open, builtin_raw_create,
    // builtin_read_all, builtin_read_chunk, builtin_read_line, builtin_seek, builtin_seek_end,
    // builtin_position, builtin_string_handle, builtin_write_handle REMOVED: used Value::Handle.
    builtin_copy_file,
    builtin_emit,
    builtin_env,
    builtin_env_has,
    builtin_exists,
    builtin_file_close,
    builtin_file_flush,
    builtin_file_open,
    builtin_file_read,
    builtin_file_seek,
    builtin_file_write,
    builtin_get_xattr,
    builtin_link,
    builtin_list_dir,
    builtin_list_xattrs,
    builtin_make_dir,
    builtin_narrow,
    builtin_path_dir,
    builtin_read_link,
    builtin_read_stdin,
    builtin_remove,
    builtin_remove_xattr,
    builtin_rename,
    builtin_revocable,
    builtin_revoke_cap,
    builtin_set_permissions,
    builtin_set_xattr,
    builtin_stat,
    builtin_stat_symlink,
    builtin_symlink,
    builtin_write,
    builtin_write_atomic,
    builtin_write_stderr,
    builtin_write_stdout,
};
// List operation implementations — live in builtins.rs.
use crate::builtins::{
    builtin_first, builtin_last, builtin_proxy, builtin_rest, builtin_reverse, builtin_sort,
};
// Async concurrency implementations.
use crate::builtins_async::{
    builtin_await, builtin_broadcast_channel, builtin_cancel_root, builtin_cancel_task,
    builtin_cancelled_q, builtin_cell_get, builtin_cell_set, builtin_channel, builtin_context,
    builtin_drain, builtin_exit_now, builtin_non_cancellable, builtin_oneshot_channel, builtin_par,
    builtin_par_filter, builtin_par_map, builtin_reactive_cell, builtin_recv, builtin_select_once,
    builtin_send, builtin_signal_channel, builtin_task, builtin_timer_channel, builtin_try_send,
    builtin_watch_channel, builtin_with_cancel, builtin_with_context, builtin_with_deadline,
    builtin_with_timeout,
};

use crate::value::{BuiltinDef, Strictness};

// Imports for core_type_env() — T-714.
use crate::type_class::ConstraintArg;
use crate::type_def::TyConDef;
use crate::types::{ClassDecl, Constraint, Kind, Row, Type, TypeEnv, TypeScheme};

use std::sync::Arc;

/// Construct `App(TyCon(name), elem)` — generic parameterized type constructor.
fn tycon_app(name: &str, elem: Type) -> Type {
    Type::App(Box::new(Type::TyCon(name.into())), Box::new(elem))
}

/// Returns all "core" module Rust builtins aggregated from the split implementation files.
///
/// Consumed exclusively by `builtin_module("core")` in `src/builtins.rs`.
/// Datetime builtins are in `datetime_builtins()` (T-715 complete).
/// Net/URI builtins are in `net_builtins()` (T-716, T-719 complete).
///
/// `reduce_dict_step` and `reduce_seq_step` are intentionally excluded: they are
/// Rust PendingBuiltin continuation helpers embedded via function pointers and
/// never looked up by name from tinct code. They must not be injected into the
/// evaluation environment.
///
/// SLOT ORDER INVARIANT: `field-get` MUST be slot 0 and `slot-get` MUST be slot 1.
/// The lowerer hardcodes these slots — do not reorder these first two entries.
///
/// The slot indices for the two dot-access builtins, exposed as constants so that
/// tests can construct `CoreExpr::Var` nodes with the correct slot without hardcoding
/// magic numbers.
#[cfg(test)]
pub const FIELD_GET_ROOT_SLOT: u32 = 0;
#[cfg(test)]
pub const SLOT_GET_ROOT_SLOT: u32 = 1;

pub fn core_builtins() -> Vec<BuiltinDef> {
    vec![
        // ── Dot-access builtins — MUST be slots 0 and 1 (lowerer invariant) ─────────
        builtin!(
            "field-get",
            builtin_field_get,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["key", "dict"]
        ),
        builtin!(
            "slot-get",
            builtin_slot_get,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["slot", "dict"]
        ),
        // ── Arithmetic ────────────────────────────────────────────────────────────────
        // Note: +, -, *, / are NOT registered here — they are multi-method dispatch
        // via Addable/Subtractable/Multipliable/Divisible instances in prelude.llt.
        // Only the builtin-* stable aliases remain as raw Rust primitives used inside
        // instance method bodies. (S-884: typeclass-env-dispatch)
        // Stable aliases (used internally by instance bodies — no dispatch)
        builtin!(
            "builtin-add",
            builtin_add,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-sub",
            builtin_sub,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-mul",
            builtin_mul,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-div",
            builtin_div_float,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        // ── Comparison ───────────────────────────────────────────────────────────────
        // Note: =, <, >, <=, >= are NOT registered here — they dispatch via
        // Equatable/Comparable instances in prelude.llt. (S-885)
        // Only builtin-* stable aliases remain as raw Rust primitives.
        // Type-specific equality primitives — used by Equatable instances.
        // No cross-type comparison; each takes exactly two args of the same type.
        builtin!(
            "builtin-eq-int",
            builtin_eq_int,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-eq-float",
            builtin_eq_float,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-eq-string",
            builtin_eq_string,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-lt",
            builtin_lt,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-gt",
            builtin_gt,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-lte",
            builtin_lte,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-gte",
            builtin_gte,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        // ── Control flow ─────────────────────────────────────────────────────────────
        builtin!(
            "builtin-if",
            builtin_if,
            [Strictness::Seq, Strictness::Id, Strictness::Id],
            1,
            ["cond", "then", "else"]
        ),
        // ── Dict primitives ──────────────────────────────────────────────────────────
        builtin!("builtin-keys", builtin_keys, [Strictness::Spine], 1, ["xs"]),
        builtin!(
            "builtin-length",
            builtin_length,
            [Strictness::Spine],
            1,
            ["xs"]
        ),
        builtin!("builtin-merge", builtin_merge, [], 0, ["a", "b"]),
        builtin!(
            "builtin-append",
            builtin_append,
            [Strictness::Id, Strictness::Seq],
            0,
            ["x", "xs"]
        ),
        builtin!(
            "builtin-get",
            builtin_get,
            [Strictness::Seq, Strictness::Spine],
            2,
            ["key", "dict"]
        ),
        builtin!(
            "builtin-has-key?",
            builtin_has_key,
            [Strictness::Seq, Strictness::Spine],
            2,
            ["key", "dict"]
        ),
        builtin!(
            "builtin-get-by-field",
            builtin_get_by_field,
            [Strictness::Seq, Strictness::Seq, Strictness::Spine],
            3,
            ["field", "key", "dict"]
        ),
        builtin!(
            "builtin-dict-has-nth?",
            builtin_dict_has_nth,
            [Strictness::Spine, Strictness::Seq],
            2,
            ["dict", "n"]
        ),
        builtin!(
            "builtin-dict-nth",
            builtin_dict_nth,
            [Strictness::Spine, Strictness::Seq],
            2,
            ["dict", "n"]
        ),
        builtin!(
            "builtin-dict-has-key-nth?",
            builtin_dict_has_key_nth,
            [Strictness::Spine, Strictness::Seq],
            2,
            ["dict", "n"]
        ),
        builtin!(
            "builtin-dict-key-nth",
            builtin_dict_key_nth,
            [Strictness::Spine, Strictness::Seq],
            2,
            ["dict", "n"]
        ),
        builtin!(
            "builtin-dict-has-kv-nth?",
            builtin_dict_has_kv_nth,
            [Strictness::Spine, Strictness::Seq],
            2,
            ["dict", "n"]
        ),
        builtin!(
            "builtin-dict-kv-nth",
            builtin_dict_kv_nth,
            [Strictness::Spine, Strictness::Seq],
            2,
            ["dict", "n"]
        ),
        builtin!(
            "builtin-build-dict",
            builtin_build_dict,
            [Strictness::Spine],
            1,
            ["entries"]
        ),
        // ── Builder ops ──────────────────────────────────────────────────────────────
        builtin!("builtin-make-builder", builtin_make_builder),
        builtin!(
            "builtin-builder-set",
            builtin_builder_set,
            [Strictness::Seq, Strictness::Id, Strictness::Seq],
            0,
            ["builder", "value", "key"]
        ),
        builtin!(
            "builtin-builder-delete",
            builtin_builder_delete,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["builder", "key"]
        ),
        builtin!(
            "builtin-builder-finish",
            builtin_builder_finish,
            [Strictness::Seq],
            1,
            ["builder"]
        ),
        builtin!(
            "builtin-builder-snapshot",
            builtin_builder_snapshot,
            [Strictness::Seq],
            1,
            ["builder"]
        ),
        builtin!(
            "builtin-builder-has?",
            builtin_builder_has,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["builder", "key"]
        ),
        builtin!(
            "builtin-builder-get",
            builtin_builder_get,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["builder", "key"]
        ),
        builtin!(
            "builtin-builder-get-or",
            builtin_builder_get_or,
            [Strictness::Seq, Strictness::Id, Strictness::Seq],
            0,
            ["builder", "default", "key"]
        ),
        // ── String ops ───────────────────────────────────────────────────────────────
        builtin!(
            "builtin-int->string",
            builtin_int_to_string,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        builtin!(
            "builtin-float->string",
            builtin_float_to_string,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        builtin!(
            // Hyphenated alias for builtin-float->string (used by type-foundations loader).
            "builtin-float-to-string",
            builtin_float_to_string,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        builtin!(
            "builtin-replace",
            builtin_replace,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["pattern", "replacement", "str"]
        ),
        builtin!("builtin-trim", builtin_trim, [Strictness::Seq], 1, ["str"]),
        builtin!(
            "builtin-str-length",
            builtin_str_length,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-str-byte-count",
            builtin_str_byte_count,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-str-has-nth-byte?",
            builtin_str_has_nth_byte,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["str", "i"]
        ),
        builtin!(
            "builtin-str-nth-byte",
            builtin_str_nth_byte,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["str", "i"]
        ),
        builtin!(
            "builtin-str-slice",
            builtin_str_slice,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["str", "start", "end"]
        ),
        builtin!(
            "builtin-str-has-nth?",
            builtin_str_has_nth,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["str", "n"]
        ),
        builtin!(
            "builtin-str-nth-char",
            builtin_str_nth_char,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["str", "n"]
        ),
        builtin!(
            "builtin-char-code",
            builtin_char_code,
            [Strictness::Seq],
            1,
            ["char"]
        ),
        builtin!("builtin-chr", builtin_chr, [Strictness::Seq], 1, ["n"]),
        builtin!(
            "builtin-str-bytes",
            builtin_str_bytes,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-bytes-str",
            builtin_bytes_str,
            [Strictness::Seq],
            1,
            ["bytes"]
        ),
        builtin!(
            "builtin-str-index-of",
            builtin_str_index_of,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["str", "needle"]
        ),
        builtin!(
            "builtin-trim-start",
            builtin_trim_start,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-trim-end",
            builtin_trim_end,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-str-to-upper-char",
            builtin_str_to_upper_char,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-str-to-lower-char",
            builtin_str_to_lower_char,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-str-map-chars",
            builtin_str_map_chars,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["f", "str"]
        ),
        builtin!(
            "builtin-regex-match?",
            builtin_regex_match,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["pattern", "str"]
        ),
        builtin!(
            "builtin-string-concat",
            builtin_string_concat,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        // ── Bytes ────────────────────────────────────────────────────────────────────
        builtin!("builtin-bytes", builtin_bytes, []),
        builtin!(
            "builtin-bytes-concat",
            builtin_bytes_concat,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-bytes-find",
            builtin_bytes_find,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["needle", "bytes"]
        ),
        builtin!("builtin-bytes-of", builtin_bytes_of, [Strictness::Seq]),
        builtin!(
            "builtin-bytes-equal?",
            builtin_bytes_equal,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-ct-equal?",
            builtin_ct_equal,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-encode",
            builtin_encode,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["encoding", "bytes"]
        ),
        builtin!(
            "builtin-bytes-get",
            builtin_bytes_get,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["bytes", "n"]
        ),
        builtin!(
            "builtin-bytes-slice",
            builtin_bytes_slice,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["bytes", "start", "end"]
        ),
        // ── Math ─────────────────────────────────────────────────────────────────────
        builtin!("builtin-floor", builtin_floor, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-round", builtin_round, [Strictness::Seq], 1, ["n"]),
        builtin!(
            "builtin-pow",
            builtin_pow,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["base", "exp"]
        ),
        builtin!("builtin-sqrt", builtin_sqrt, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-log", builtin_log, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-log2", builtin_log2, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-log10", builtin_log10, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-exp", builtin_exp, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-sin", builtin_sin, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-cos", builtin_cos, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-tan", builtin_tan, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-asin", builtin_asin, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-acos", builtin_acos, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-atan", builtin_atan, [Strictness::Seq], 1, ["n"]),
        builtin!(
            "builtin-atan2",
            builtin_atan2,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["y", "x"]
        ),
        builtin!(
            "builtin-nan?",
            builtin_nan_check,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        builtin!(
            "builtin-inf?",
            builtin_inf_check,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        builtin!(
            "builtin-finite?",
            builtin_finite_check,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        // ── Bitwise ──────────────────────────────────────────────────────────────────
        builtin!(
            "builtin-band",
            builtin_band,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-bor",
            builtin_bor,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-bxor",
            builtin_bxor,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-shl",
            builtin_shl,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["n", "bits"]
        ),
        builtin!(
            "builtin-shr",
            builtin_shr,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["n", "bits"]
        ),
        // ── Type conversion ──────────────────────────────────────────────────────────
        builtin!("builtin-float", builtin_float, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-to-int", builtin_to_int, [Strictness::Seq]),
        builtin!("builtin-to-float", builtin_to_float, [Strictness::Seq]),
        // ── Evaluation control ───────────────────────────────────────────────────────
        builtin!(
            "builtin-materialize",
            builtin_force,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!(
            "builtin-raise",
            builtin_raise,
            [Strictness::Seq],
            0,
            ["msg"]
        ),
        builtin!(
            "builtin-macro-error",
            builtin_macro_error,
            [Strictness::Seq, Strictness::Id]
        ),
        builtin!("builtin-try", builtin_try, [Strictness::Id], 1, ["f"]),
        builtin!(
            "builtin-apply",
            builtin_apply,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["f", "args"]
        ),
        builtin!("builtin-until", builtin_until),
        // ── Typeclass dispatch ──────────────────────────────────────────────────────
        // ── Type introspection ───────────────────────────────────────────────────────
        builtin!(
            "builtin-type-of",
            builtin_type_of,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!("builtin-ast-of", builtin_ast_of, [Strictness::Id], 0, ["x"]),
        builtin!(
            "builtin-check-type",
            builtin_check_type,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["type-name", "x"]
        ),
        // ── Caps/environment introspection ───────────────────────────────────────────
        builtin!(
            "builtin-cap-env-has?",
            builtin_cap_env_has,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["name", "env"]
        ),
        // ── Schema validation ────────────────────────────────────────────────────────
        builtin!(
            "builtin-validate",
            builtin_validate,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["schema", "x"]
        ),
        // ── I/O ──────────────────────────────────────────────────────────────────────
        builtin!("builtin-emit", builtin_emit, [Strictness::Seq], 0, ["x"]),
        builtin!("builtin-env", builtin_env, [Strictness::Seq], 0, ["name"]),
        builtin!(
            "builtin-env-has?",
            builtin_env_has,
            [Strictness::Seq],
            0,
            ["name"]
        ),
        // builtin-open, builtin-string-handle, builtin-read-line, builtin-read-chunk,
        // builtin-cap-data, builtin-write-handle, builtin-flush, builtin-close,
        // builtin-raw-create, builtin-seek, builtin-seek-end, builtin-position
        // REMOVED: All operated on Value::Handle/WriteHandle which no longer exist.
        builtin!(
            "builtin-narrow",
            builtin_narrow,
            [Strictness::Seq, Strictness::Seq],
            0,
            ["cap", "path"]
        ),
        builtin!(
            "builtin-revocable",
            builtin_revocable,
            [Strictness::Seq],
            0,
            ["cap"]
        ),
        builtin!(
            "builtin-revoke-cap",
            builtin_revoke_cap,
            [Strictness::Seq],
            0,
            ["cap"]
        ),
        builtin!(
            "builtin-write",
            builtin_write,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["cap", "path", "content"]
        ),
        builtin!(
            "builtin-write-atomic",
            builtin_write_atomic,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["cap", "path", "content"]
        ),
        builtin!(
            "builtin-list-dir",
            builtin_list_dir,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["cap", "path"]
        ),
        builtin!(
            "builtin-stat",
            builtin_stat,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["cap", "path"]
        ),
        builtin!(
            "builtin-exists",
            builtin_exists,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["cap", "path"]
        ),
        builtin!(
            "builtin-stat-symlink",
            builtin_stat_symlink,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["cap", "path"]
        ),
        builtin!(
            "builtin-copy-file",
            builtin_copy_file,
            [
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq
            ],
            4,
            ["src-cap", "src-path", "dst-cap", "dst-path"]
        ),
        builtin!(
            "builtin-symlink",
            builtin_symlink,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["cap", "path", "target"]
        ),
        builtin!(
            "builtin-set-permissions",
            builtin_set_permissions,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["cap", "path", "mode"]
        ),
        builtin!(
            "builtin-get-xattr",
            builtin_get_xattr,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["cap", "path", "name"]
        ),
        builtin!(
            "builtin-set-xattr",
            builtin_set_xattr,
            [
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq
            ],
            4,
            ["cap", "path", "name", "value"]
        ),
        builtin!(
            "builtin-remove-xattr",
            builtin_remove_xattr,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["cap", "path", "name"]
        ),
        builtin!(
            "builtin-list-xattrs",
            builtin_list_xattrs,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["cap", "path"]
        ),
        builtin!(
            "builtin-make-dir",
            builtin_make_dir,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["cap", "path"]
        ),
        builtin!(
            "builtin-remove",
            builtin_remove,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["cap", "path"]
        ),
        builtin!(
            "builtin-rename",
            builtin_rename,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["cap", "from", "to"]
        ),
        builtin!(
            "builtin-link",
            builtin_link,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["cap", "path", "target"]
        ),
        builtin!(
            "builtin-read-link",
            builtin_read_link,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["cap", "path"]
        ),
        // builtin-read-all removed: operated on Value::Handle which no longer exists.
        builtin!(
            "builtin-path-dir",
            builtin_path_dir,
            [Strictness::Seq],
            1,
            ["path"]
        ),
        // ── File primitives (Value::File — thin OS wrappers, no buffering) ───────────
        builtin!(
            "builtin-file-open",
            builtin_file_open,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["cap", "path", "mode"]
        ),
        builtin!(
            "builtin-file-read",
            builtin_file_read,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["file", "n"]
        ),
        builtin!(
            "builtin-file-write",
            builtin_file_write,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["file", "bytes"]
        ),
        builtin!(
            "builtin-file-flush",
            builtin_file_flush,
            [Strictness::Seq],
            0,
            ["file"]
        ),
        builtin!(
            "builtin-file-close",
            builtin_file_close,
            [Strictness::Seq],
            0,
            ["file"]
        ),
        builtin!(
            "builtin-file-seek",
            builtin_file_seek,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["file", "pos"]
        ),
        // ── Stateless stdio primitives ────────────────────────────────────────────────
        builtin!(
            "builtin-write-stdout",
            builtin_write_stdout,
            [Strictness::Seq, Strictness::Id],
            2,
            ["sep", "x"]
        ),
        builtin!(
            "builtin-write-stderr",
            builtin_write_stderr,
            [Strictness::Seq, Strictness::Id],
            2,
            ["sep", "x"]
        ),
        builtin!("builtin-read-stdin", builtin_read_stdin, [Strictness::Seq]),
        // ── Decomposed include primitives ─────────────────────────────────────────────
        builtin!(
            "builtin-blake3",
            builtin_blake3,
            [Strictness::Seq],
            0,
            ["bytes"]
        ),
        builtin!(
            "builtin-cap-identity",
            builtin_cap_identity,
            [Strictness::Seq],
            0,
            ["cap"]
        ),
        builtin!("builtin-load", builtin_load, [Strictness::Seq], 1, ["path"]),
        // ── 4-stage pipeline primitives ───────────────────────────────────────────────
        // Stage 1: builtin-parse  — Bytes + path → raw Program (parse only)
        builtin!(
            "builtin-parse",
            builtin_parse,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["bytes", "path"]
        ),
        // Stage 3: builtin-resolve — expand + desugar + resolve (no typecheck)
        builtin!(
            "builtin-resolve",
            builtin_resolve,
            [Strictness::Seq],
            1,
            ["doc"]
        ),
        // Stage 4: builtin-typecheck — typecheck a resolved Program, update TypeContext
        // Accepts 1 or 2 args: program, [type-ctx]. Marked variadic; arity checked inside.
        builtin!(
            "builtin-typecheck",
            builtin_typecheck,
            [Strictness::Seq],
            1,
            ["program"]
        ),
        // TypeContext primitives
        builtin!("builtin-get-type-context", builtin_get_type_context, [], 0),
        builtin!("builtin-make-type-ctx", builtin_make_type_ctx, [], 0),
        builtin!(
            "builtin-fork-type-ctx",
            builtin_fork_type_ctx,
            [Strictness::Seq],
            1,
            ["type-ctx"]
        ),
        builtin!(
            "builtin-program",
            builtin_program,
            [Strictness::Spine],
            1,
            ["docs"]
        ),
        builtin!(
            "builtin-module",
            builtin_builtin_module,
            [Strictness::Seq],
            1,
            ["name"]
        ),
        builtin!("builtin-eval", builtin_eval, [Strictness::Seq], 1, ["doc"]),
        builtin!(
            "builtin-eval-repr",
            builtin_eval_repr,
            [Strictness::Seq],
            1,
            ["x"]
        ),
        builtin!(
            "builtin-variant-payload",
            builtin_variant_payload,
            [Strictness::Seq],
            1,
            ["variant"]
        ),
        builtin!(
            "builtin-extend-env",
            builtin_extend_env,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["parent", "entries"]
        ),
        // builtin-current-env: zero-arg; returns the calling lexical environment.
        // No force_count or pos_strictness — it inspects no arguments.
        builtin!("builtin-current-env", builtin_current_env, [], 0),
        // builtin-eval-macro-ast: evaluate a macro-produced Expr.* AST in the call-site scope.
        // Reads ᴍᴀᴄʀᴏ∷env and ᴍᴀᴄʀᴏ∷span from caller_env (injected by @Expr dispatch).
        // pos_strictness[0] = Seq: the Expr.* arg must be materialized before conversion.
        // force_count = 1: force the first positional arg before dispatching to the builtin.
        builtin!(
            "builtin-eval-macro-ast",
            builtin_eval_macro_ast,
            [Strictness::Seq],
            1,
            ["ast"]
        ),
        builtin!(
            "builtin-eval-types",
            builtin_eval_types,
            [Strictness::Seq],
            1,
            ["program"]
        ),
        builtin!(
            "builtin-include-cache-get",
            builtin_include_cache_get,
            [Strictness::Seq],
            1,
            ["key"]
        ),
        builtin!(
            "builtin-include-cache-put",
            builtin_include_cache_put,
            [],
            2,
            ["key", "value"]
        ),
        // ── Sequences — transforms ─────────────────────────────────────────────────────
        builtin!(
            "builtin-take",
            builtin_take,
            [Strictness::Seq, Strictness::Spine],
            2,
            ["n", "xs"]
        ),
        builtin!(
            "builtin-drop",
            builtin_drop,
            [Strictness::Seq, Strictness::Spine],
            2,
            ["n", "xs"]
        ),
        builtin!(
            "builtin-concat",
            builtin_concat,
            [Strictness::Spine, Strictness::Seq],
            1,
            ["a", "b"]
        ),
        // ── Sequences — list operations ───────────────────────────────────────────────
        builtin!(
            "builtin-first",
            builtin_first,
            [Strictness::Spine],
            0,
            ["xs"]
        ),
        builtin!("builtin-last", builtin_last, [Strictness::Spine], 0, ["xs"]),
        builtin!("builtin-rest", builtin_rest, [Strictness::Spine], 0, ["xs"]),
        builtin!(
            "builtin-reverse",
            builtin_reverse,
            [Strictness::Spine],
            0,
            ["xs"]
        ),
        builtin!(
            "builtin-sort",
            builtin_sort,
            [Strictness::Spine, Strictness::Spine],
            0,
            ["cmp", "xs"]
        ),
        // ── Async concurrency ─────────────────────────────────────────────────────────
        builtin!("builtin-task", builtin_task),
        builtin!("builtin-await", builtin_await),
        builtin!("builtin-channel", builtin_channel),
        builtin!("builtin-send", builtin_send),
        builtin!("builtin-recv", builtin_recv),
        builtin!("builtin-broadcast-channel", builtin_broadcast_channel),
        builtin!("builtin-oneshot-channel", builtin_oneshot_channel),
        builtin!("builtin-try-send", builtin_try_send),
        builtin!("builtin-select-once", builtin_select_once),
        builtin!("builtin-par", builtin_par),
        builtin!("builtin-par-map", builtin_par_map),
        builtin!("builtin-par-filter", builtin_par_filter),
        builtin!("builtin-signal-channel", builtin_signal_channel),
        builtin!(
            "builtin-timer-channel",
            builtin_timer_channel,
            [Strictness::Seq],
            1,
            ["duration"]
        ),
        builtin!("builtin-watch-channel", builtin_watch_channel),
        builtin!("builtin-context", builtin_context),
        builtin!(
            "builtin-with-cancel",
            builtin_with_cancel,
            [Strictness::Seq],
            0,
            ["ctx"]
        ),
        builtin!(
            "builtin-with-timeout",
            builtin_with_timeout,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            1,
            ["ctx", "duration", "f"]
        ),
        builtin!(
            "builtin-with-deadline",
            builtin_with_deadline,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            1,
            ["ctx", "deadline", "f"]
        ),
        builtin!(
            "builtin-cancelled-q",
            builtin_cancelled_q,
            [Strictness::Seq],
            0,
            ["ctx"]
        ),
        builtin!(
            "builtin-cancel-task",
            builtin_cancel_task,
            [Strictness::Seq],
            0,
            ["task"]
        ),
        builtin!("builtin-non-cancellable", builtin_non_cancellable),
        builtin!(
            "builtin-with-context",
            builtin_with_context,
            [Strictness::Seq, Strictness::Id],
            1,
            ["ctx", "f"]
        ),
        builtin!("builtin-cancel-root", builtin_cancel_root),
        builtin!("builtin-drain", builtin_drain),
        builtin!(
            "builtin-exit-now",
            builtin_exit_now,
            [Strictness::Seq],
            0,
            ["code"]
        ),
        // ── Reactive cells (T-831) ────────────────────────────────────────────────────
        builtin!("builtin-reactive-cell", builtin_reactive_cell),
        builtin!("builtin-cell-get", builtin_cell_get),
        builtin!("builtin-cell-set", builtin_cell_set),
        // ── Meta / reflection ─────────────────────────────────────────────────────────
        builtin!(
            "builtin-gensym",
            builtin_gensym,
            [Strictness::Seq, Strictness::Seq],
            0,
            ["prefix", "n"]
        ),
        builtin!(
            "builtin-llt-repr",
            builtin_llt_repr,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!(
            "builtin-to-tinct",
            builtin_to_tinct,
            [Strictness::Seq],
            1,
            ["x"]
        ),
        builtin!(
            "builtin-tag-of",
            builtin_tag_of,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!(
            "builtin-span-of",
            builtin_span_of,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!(
            "builtin-var-resolution",
            builtin_var_resolution,
            [Strictness::Seq, Strictness::Seq],
            0,
            ["offset", "env"]
        ),
        builtin!(
            "builtin-annotation-of",
            builtin_annotation_of,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!(
            "builtin-make-annotated",
            builtin_make_annotated,
            [Strictness::Seq, Strictness::Seq],
            0,
            ["value", "annotation"]
        ),
        // S-861: equirecursive-checker — contractiveness check for mu combinator.
        // Used by stdlib/prelude.llt type-stage `mu` to validate TypeNode.Recursive bodies.
        // Also called from expand_named in typecheck_annot.rs (wired in S-861, both still
        // dead code pending annotation resolver wiring in S-862).
        builtin!(
            "builtin-is-contractive",
            builtin_is_contractive,
            [Strictness::Seq],
            0,
            ["type-node"]
        ),
        builtin!(
            "builtin-decimal",
            builtin_decimal,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!(
            "builtin-big-int",
            builtin_big_int,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!("builtin-proxy", builtin_proxy),
        builtin!(
            "builtin-macro-injects",
            builtin_macro_injects,
            [Strictness::Seq],
            0,
            ["macro-env"]
        ),
        // ── Boot-level AST construction ───────────────────────────────────────────────
        // builtin-sequential: used by boot-level macros (>>, loader.llt) to construct
        // a Sequential AST node before the prelude's Expr type is in scope.
        builtin!(
            "builtin-sequential",
            builtin_sequential,
            [Strictness::Seq],
            0,
            ["exprs"]
        ),
        // builtin-ast-to-program: convert Expr.* AST node to Value::Program
        builtin!(
            "builtin-ast-to-program",
            builtin_ast_to_program,
            [Strictness::Seq],
            1,
            ["ast"]
        ),
    ]
}

/// Populate `env` with the type signatures for all "core" module builtins.
///
/// Contains everything EXCEPT datetime types and net/URI types, which are in their own modules.
///
/// ## Exclusions (in their own modules)
///
/// **Datetime** (`src/builtins_datetime.rs` → `datetime_type_env()`):
/// `parse-timestamp`, `format-timestamp`, `timestamp->unix`, `unix->timestamp`, `now`,
/// `fixed-clock`, `timestamp-add`, `timestamp-diff`, `timestamp<?`/`>?`/`=?`,
/// `timestamp-year`/`-month`/`-day`/`-hour`/`-minute`/`-second`, `timestamp-parts`,
/// `duration-nanos`/`-seconds`/`-minutes`/`-hours`/`-days`, `duration->seconds`/`->nanos`,
/// `load-tz`, `timestamp-in-tz`, `local->timestamp`, `local-tz-name`.
///
/// **Net/URI** (`src/builtins_net.rs` → `net_type_env()`):
/// `connect`, `send-datagram`, `recv-datagram`, `tls-layer`, `tls-peer-cert`,
/// `quic-session`, `quic-open-stream`, `quic-open-datagram`, `http2-session`,
/// `http3-session`, `http-request`, `icmp-ping`, `uri`, `url`, `urn`.
/// Type aliases: `QuicSession`, `Http2Session`, `Http3Session`, `QuicDatagramHandle`,
/// `DatagramHandle`, `Url`.
pub fn core_type_env(env: &mut TypeEnv) {
    // ── Dot-access builtins ───────────────────────────────────────────────────────────
    // field-get: (String|Int) → Any → Any  (string/int key lookup; target may be Dict, Proxy, Variant, etc.)
    env.insert(
        "field-get".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );
    // slot-get: Int → Any → Any  (positional slot lookup in Dict or Environment)
    env.insert(
        "slot-get".to_string(),
        Type::Function {
            params: vec![(None, Type::Int), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );

    // These ClassDecl Arcs are used below for builtin-concat, builtin-get, get, and get? schemes.
    // They must carry full FD information so constraint improvement fires correctly at call sites.
    let indexable_class = Arc::new(ClassDecl {
        name: "Indexable".to_string(),
        params: vec![
            ("container".to_string(), Kind::Type),
            ("key".to_string(), Kind::Type),
            ("value".to_string(), Kind::Type),
        ],
        superclasses: vec![],
        determines: vec![(vec![0, 1], vec![2])], // (container, key) → value
        resolver: None,
        resolver_injective: false,
        method_signatures: vec![],
    });

    let concatable_class = Arc::new(ClassDecl {
        name: "Concatable".to_string(),
        params: vec![
            ("a".to_string(), Kind::Type),
            ("b".to_string(), Kind::Type),
            ("c".to_string(), Kind::Type),
        ],
        superclasses: vec![],
        determines: vec![(vec![0, 1], vec![2])], // (a, b) → c
        resolver: None,
        resolver_injective: false,
        method_signatures: vec![],
    });

    // ── Dict primitives ───────────────────────────────────────────────────────
    // builtin-keys: Record({...}) → Seq(Int | Str)
    // Dict keys can be either integer (seq-style) or string (record-style).
    env.insert(
        "builtin-keys".to_string(),
        Type::Function {
            params: vec![(
                None,
                Type::Record(Row {
                    fields: indexmap::IndexMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }),
            )],
            ret: Box::new(tycon_app(
                "Seq",
                Type::normalize_union(vec![Type::Int, Type::Str]),
            )),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-length: Top → Int
    // Accepts Seq, Dict, Str, or Bytes. Using Top avoids false-positive type errors.
    env.insert(
        "builtin-length".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-merge: Record({}) → Record({}) → Record({})
    // Right-biased merge of two dicts.
    env.insert(
        "builtin-merge".to_string(),
        Type::Function {
            params: vec![
                (
                    None,
                    Type::Record(Row {
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ),
                (
                    None,
                    Type::Record(Row {
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ),
            ],
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-append: Record({}) → Top → Record({})
    // Appends a value to a dict (integer-indexed).
    env.insert(
        "builtin-append".to_string(),
        Type::Function {
            params: vec![
                (
                    None,
                    Type::Record(Row {
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ),
                (None, Type::Any),
            ],
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-dict-nth: Dict → Int → Top | Absent
    env.insert(
        "builtin-dict-nth".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Int)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-dict-key-nth: Dict → Int → (Int | Str) | Absent
    env.insert(
        "builtin-dict-key-nth".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Int)],
            ret: Box::new(Type::normalize_union(vec![Type::Int, Type::Str, Type::Any])),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-dict-kv-nth: Dict → Int → {key: Top, value: Top} | Absent
    {
        let mut kv_fields = indexmap::IndexMap::new();
        kv_fields.insert(
            "key".to_string(),
            Type::normalize_union(vec![Type::Int, Type::Str]),
        );
        kv_fields.insert("value".to_string(), Type::Any);
        env.insert(
            "builtin-dict-kv-nth".to_string(),
            Type::Function {
                params: vec![(None, Type::Any), (None, Type::Int)],
                ret: Box::new(Type::Any),
                variadic: false,
                required_count: 2,
            },
        );
    }

    // ── String operations ─────────────────────────────────────────────────────
    // builtin-str: variadic Top... → Str
    // Accepts any number of arguments (stringifies each and concatenates).
    env.insert(
        "builtin-str".to_string(),
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Str),
            variadic: true,
            required_count: 0,
        },
    );
    // builtin-split: Str → Str → Seq(Str)
    // Args: separator, input string. Returns a Seq of the split substrings.
    env.insert(
        "builtin-split".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Str)],
            ret: Box::new(tycon_app("Seq", Type::Str)),
            variadic: false,
            required_count: 2,
        },
    );
    // int->string: Int -> Str  (primitive backing Printable; no typeclass constraint)
    for name in ["int->string", "builtin-int->string"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Int)],
                ret: Box::new(Type::Str),
                variadic: false,
                required_count: 1,
            },
        );
    }
    // float->string: Float -> Str  (primitive backing Printable; no typeclass constraint)
    // builtin-float-to-string: hyphenated alias used by type-foundations loader.llt.
    for name in [
        "float->string",
        "builtin-float->string",
        "builtin-float-to-string",
    ] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Float)],
                ret: Box::new(Type::Str),
                variadic: false,
                required_count: 1,
            },
        );
    }
    // builtin-string-concat: Str -> Str -> Str  (primitive two-arg string concatenation)
    env.insert(
        "builtin-string-concat".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Str)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-json-parse: String → Top (parsed JSON value, type is dynamic)
    env.insert(
        "builtin-json-parse".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 1,
        },
    );
    // ── Bytes ─────────────────────────────────────────────────────────────────
    // builtin-bytes: variadic Bytes → Bytes (concat)
    env.insert(
        "builtin-bytes".to_string(),
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Bytes),
            variadic: true,
            required_count: 0,
        },
    );
    // builtin-bytes-concat: Bytes → Bytes → Bytes (binary concat, O(n))
    env.insert(
        "builtin-bytes-concat".to_string(),
        Type::Function {
            params: vec![(None, Type::Bytes), (None, Type::Bytes)],
            ret: Box::new(Type::Bytes),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-bytes-find: Bytes → Bytes → Int
    env.insert(
        "builtin-bytes-find".to_string(),
        Type::Function {
            params: vec![(None, Type::Bytes), (None, Type::Bytes)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-bytes-of: Seq → Bytes (or Dict → Bytes)
    env.insert(
        "builtin-bytes-of".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)], // Accepts Seq or Dict
            ret: Box::new(Type::Bytes),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-bytes-equal?: Bytes → Bytes → Bool
    env.insert(
        "builtin-bytes-equal?".to_string(),
        Type::Function {
            params: vec![(None, Type::Bytes), (None, Type::Bytes)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-ct-equal?: Bytes → Bytes → Bool
    env.insert(
        "builtin-ct-equal?".to_string(),
        Type::Function {
            params: vec![(None, Type::Bytes), (None, Type::Bytes)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-encode: ByteOrder → (Int|Float) → Bytes
    // ByteOrder is a nominal variant type declared in prelude.llt.
    // Accepts Any for the format arg (variant tag dispatch at runtime).
    env.insert(
        "builtin-encode".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Bytes),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-bytes-get: Int → Bytes → Int (O(1) random access, returns byte as 0–255)
    env.insert(
        "builtin-bytes-get".to_string(),
        Type::Function {
            params: vec![(None, Type::Int), (None, Type::Bytes)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-bytes-slice: Bytes → Int → Int → Bytes (O(1) zero-copy subslice)
    env.insert(
        "builtin-bytes-slice".to_string(),
        Type::Function {
            params: vec![(None, Type::Bytes), (None, Type::Int), (None, Type::Int)],
            ret: Box::new(Type::Bytes),
            variadic: false,
            required_count: 3,
        },
    );

    // ── Numeric operations ────────────────────────────────────────────────────
    // Math functions: 1-arg (Number -> Float)
    for name in [
        "sqrt", "log", "log2", "log10", "exp", "sin", "cos", "tan", "asin", "acos", "atan",
    ] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::normalize_union(vec![Type::Int, Type::Float]))],
                ret: Box::new(Type::Float),
                variadic: false,
                required_count: 1,
            },
        );
    }
    // Math functions: 2-arg (Number -> Number -> Float)
    for name in ["pow", "atan2"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![
                    (None, Type::normalize_union(vec![Type::Int, Type::Float])),
                    (None, Type::normalize_union(vec![Type::Int, Type::Float])),
                ],
                ret: Box::new(Type::Float),
                variadic: false,
                required_count: 2,
            },
        );
    }
    // Float predicates (Float -> Bool)
    for name in ["nan?", "inf?", "finite?"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Float)],
                ret: Box::new(Type::Int),
                variadic: false,
                required_count: 1,
            },
        );
    }
    // Bitwise shift operations (Int -> Int -> Int)
    for name in ["shl", "shr"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Int), (None, Type::Int)],
                ret: Box::new(Type::Int),
                variadic: false,
                required_count: 2,
            },
        );
    }
    // float: Number → Float (converts Int or Float to Float)
    env.insert(
        "float".to_string(),
        Type::Function {
            params: vec![(None, Type::normalize_union(vec![Type::Int, Type::Float]))],
            ret: Box::new(Type::Float),
            variadic: false,
            required_count: 1,
        },
    );

    // ── Error handling ────────────────────────────────────────────────────────
    // builtin-try: Top → Top
    // Takes a zero-arg function, evaluates it, catches runtime errors.
    // Returns a nominal variant Result.Ok(value) or Result.Error(message).
    // Return type is Top (not structural union) to avoid T004 false positives when
    // user code matches on constructor patterns [Result.Ok v] / [Result.Error msg].
    env.insert(
        "builtin-try".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 1,
        },
    );

    // ── Evaluation control ────────────────────────────────────────────────────
    env.insert(
        "builtin-materialize".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 1,
        },
    );

    // ── Control flow — builtin-if ─────────────────────────────────────────────
    // builtin-if: Bool → Top → Top → Top
    // Three-argument conditional: condition, then-branch, else-branch.
    // Return type is Top (union of branch types depends on runtime choice).
    env.insert(
        "builtin-if".to_string(),
        Type::Function {
            params: vec![(None, Type::Int), (None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 3,
        },
    );

    // ── Type introspection — builtin-type-of, builtin-tag-of ─────────────────
    // builtin-type-of: Top → Str
    // Returns the runtime type name of any value as a string.
    env.insert(
        "builtin-type-of".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-tag-of: Top → Str
    // Returns the variant tag string from a Variant or Expression value.
    // Not in meta_builtin_types (that module copies from core_type_env).
    // Must be here so prelude's `tag-of` body type-checks correctly.
    env.insert(
        "builtin-tag-of".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 1,
        },
    );

    // ── Error raising — builtin-raise ────────────────────────────────────────
    // builtin-raise: Str → Never
    // Raises a user error with the given message. Never returns.
    env.insert(
        "builtin-raise".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Never),
            variadic: false,
            required_count: 1,
        },
    );

    // ── Output — builtin-emit ─────────────────────────────────────────────────
    // builtin-emit: Top → {}
    // Writes value to stdout. Side effect; returns null (empty dict).
    let null_ty = Type::Record(Row {
        fields: indexmap::IndexMap::new(),
        tail: crate::type_def::RowTail::Empty,
    });
    env.insert(
        "builtin-emit".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(null_ty.clone()),
            variadic: false,
            required_count: 1,
        },
    );

    // ── Environment — builtin-env, builtin-env-has? ──────────────────────────
    // builtin-env: Str → Str
    // Reads an environment variable by name. Raises if unset or disallowed.
    // Prelude wraps this with builtin-env-has? to implement the user-facing `env`.
    env.insert(
        "builtin-env".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 1,
        },
    );

    // builtin-env-has?: Str → Int
    // Returns 1 if the env var is set and allowed, 0 otherwise.
    env.insert(
        "builtin-env-has?".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 1,
        },
    );

    // ── Type checking — builtin-check-type ───────────────────────────────────
    // builtin-check-type: Str → Any → Any
    // Validates that the second argument matches the named type; returns the value on success,
    // raises on mismatch. Used by tinct-side expects validation (T-1506).
    env.insert(
        "builtin-check-type".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );

    // ── Caps/environment introspection — builtin-cap-env-has? ─────────────────
    // builtin-cap-env-has?: Str → Environment → Bool
    // Returns Boolean.True if the named capability is present in the given tinct
    // runtime environment, Boolean.False otherwise. Used by tinct-side caps enforcement
    // (T-1507).
    env.insert(
        "builtin-cap-env-has?".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );

    // ── Function application — builtin-apply ──────────────────────────────────
    // builtin-apply: Top → Top → Top
    // Applies a function to a dict of arguments. Return type is Top (dynamic dispatch).
    env.insert(
        "builtin-apply".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );

    // ── Schema validation ────────────────────────────────────────────────────
    // builtin-validate: takes a schema dict (Record({})) and any value, returns the
    // value if valid (or raises). The return type is Top — builtin-validate is
    // identity-like but can't express dependent types (schema→value→value).
    env.insert(
        "builtin-validate".to_string(),
        Type::Function {
            params: vec![
                (
                    None,
                    Type::Record(Row {
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ),
                (None, Type::Any),
            ],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );

    // ── Macro support ─────────────────────────────────────────────────────────
    // builtin-macro-error: takes a message string and an optional AST node,
    // raises a compile-time error. If node is provided and is an Expression,
    // uses its span; otherwise uses call site span. Never returns.
    env.insert(
        "builtin-macro-error".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Str),
                (None, Type::Unknown), // optional AST node
            ],
            ret: Box::new(Type::Never),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-macro-injects: takes a macro name (any value, looked up at
    // runtime) and returns the sequence of injected values for that macro.
    env.insert(
        "builtin-macro-injects".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(tycon_app("Seq", Type::Any)),
            variadic: false,
            required_count: 1,
        },
    );

    // ── Type introspection ────────────────────────────────────────────────────
    // These accept any value (Top), return Str
    env.insert(
        "to-tinct".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 1,
        },
    );
    // annotation-of: takes any value, returns its annotation dict (or {} if none).
    // Returns Unknown (not Top) so that dot-access on the result works gracefully
    // under gradual typing — ann.doc, ann.version etc. resolve to Unknown, not an error.
    env.insert(
        "annotation-of".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Unknown),
            variadic: false,
            required_count: 1,
        },
    );
    // make-annotated: wraps a value in Value::Annotated with the given annotation dict.
    // Returns Top — the annotated wrapper is transparent to the type system.
    env.insert(
        "make-annotated".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-module: returns a dict of builtins for the named module.
    env.insert(
        "builtin-module".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Any), // Returns a Dict of builtins
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-eval: evaluate a Document/Program/Seq of expressions with optional env/input.
    // Return type is Top — genuinely opaque (output depends on runtime values).
    env.insert(
        "builtin-eval".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: true,
            required_count: 1,
        },
    );
    // builtin-load: parse tinct source text into a Program value.
    // Accepts optional named args: name: String, hash: String (via variadic).
    env.insert(
        "builtin-load".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Any), // Returns a Program (AST value)
            variadic: true,
            required_count: 1,
        },
    );
    // builtin-parse: parse Bytes + path → raw Program (no expansion/resolution).
    env.insert(
        "builtin-parse".to_string(),
        Type::Function {
            params: vec![(None, Type::Bytes), (None, Type::Str)],
            ret: Box::new(Type::Any), // Returns a Program
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-resolve: expand + desugar + resolve a raw Program → resolved Program.
    env.insert(
        "builtin-resolve".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // Returns a resolved Program
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-typecheck: type-check a resolved Program, returns typed Program.
    // Accepts 1 or 2 positional args (program, [type-ctx]).
    env.insert(
        "builtin-typecheck".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Any), // Returns a typed Program
            variadic: true,
            required_count: 1,
        },
    );
    // builtin-get-type-context: retrieve the current TypeContext (zero or one arg).
    // One-arg form forces the argument for its side effects, then returns TypeContext.
    env.insert(
        "builtin-get-type-context".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // Returns an opaque TypeContext handle
            variadic: true,
            required_count: 0,
        },
    );
    // builtin-make-type-ctx: create a fresh TypeContext seeded with core type defs.
    env.insert(
        "builtin-make-type-ctx".to_string(),
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Any), // Returns an opaque TypeContext handle
            variadic: false,
            required_count: 0,
        },
    );
    // builtin-fork-type-ctx: create a child TypeContext from a parent.
    env.insert(
        "builtin-fork-type-ctx".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // Returns an opaque TypeContext handle
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-blake3: compute blake3 hash of a string. Returns a hex string.
    env.insert(
        "builtin-blake3".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-cap-identity: return a stable string identity for a DirCap (for cache keys).
    env.insert(
        "builtin-cap-identity".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-include-cache-get: look up a content-addressed include result.
    env.insert(
        "builtin-include-cache-get".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-include-cache-put: store/update a content-addressed include result.
    env.insert(
        "builtin-include-cache-put".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-read-line and builtin-read-chunk type env entries removed (operated on Value::Handle).
    // builtin-read-all and write-handle type env entries removed (operated on Value::Handle).
    // builtin-exists: check whether a path exists under a capability.
    env.insert(
        "builtin-exists".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-stat-symlink: stat a path without following symlinks.
    // Returns the same shape as stat.
    env.insert(
        "builtin-stat-symlink".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::from_iter([
                    ("name".to_string(), Type::Str),
                    ("kind".to_string(), Type::Str),
                    ("size".to_string(), Type::Int),
                ]),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-copy-file: copy src-cap/src-path to dst-cap/dst-path.
    // Returns Null on success.
    env.insert(
        "builtin-copy-file".to_string(),
        Type::Function {
            params: vec![
                (None, Type::DirCap),
                (None, Type::Str),
                (None, Type::DirCap),
                (None, Type::Str),
            ],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 4,
        },
    );
    // builtin-symlink: create a symlink at cap/path pointing to target.
    // Returns Null on success.
    env.insert(
        "builtin-symlink".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 3,
        },
    );
    // builtin-set-permissions: set file mode bits at cap/path.
    // Returns Null on success.
    env.insert(
        "builtin-set-permissions".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Int)],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 3,
        },
    );
    // builtin-get-xattr: read an extended attribute (xattr) by name.
    // Returns the attribute value as a Str.
    env.insert(
        "builtin-get-xattr".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 3,
        },
    );
    // builtin-set-xattr: write an extended attribute (xattr) by name.
    // Returns Null on success.
    env.insert(
        "builtin-set-xattr".to_string(),
        Type::Function {
            params: vec![
                (None, Type::DirCap),
                (None, Type::Str),
                (None, Type::Str),
                (None, Type::Str),
            ],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 4,
        },
    );
    // builtin-remove-xattr: delete an extended attribute (xattr) by name.
    // Returns Null on success.
    env.insert(
        "builtin-remove-xattr".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 3,
        },
    );
    // builtin-list-xattrs: list all extended attribute names at cap/path.
    // Returns a sequence of attribute name strings.
    env.insert(
        "builtin-list-xattrs".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(tycon_app("Seq", Type::Str)),
            variadic: false,
            required_count: 2,
        },
    );
    env.insert(
        "builtin-remove".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 2,
        },
    );
    env.insert(
        "from-json".to_string(),
        Type::Function {
            params: vec![(
                None,
                Type::Str, // Accepts String only (Handle path removed — Value::Handle gone)
            )],
            // Top: JSON parse output can be any JSON value (object, array,
            // string, number, bool, null). A precise type requires schema information.
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 1,
        },
    );

    // builtin-narrow: DirCap → Top → DirCap  (attenuate capability to subdirectory or permissions)
    // Second argument is String (subdirectory path) or DirCapFlag (permission restriction).
    // Top covers both cases. Prelude aliases this as `narrow`.
    env.insert(
        "builtin-narrow".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Any)],
            ret: Box::new(Type::DirCap),
            variadic: true,
            required_count: 2,
        },
    );
    // builtin-revocable: DirCap → Top  (wrap a DirCap in a revocable wrapper)
    env.insert(
        "builtin-revocable".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 1,
        },
    );

    // builtin-build-dict: Top → Record({})
    env.insert(
        "builtin-build-dict".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 1,
        },
    );
    // ── Sequences: transforms ──────────────────────────────────────────────────────────────────
    // builtin-map: ∀a b. (a → b) → Seq(a) → Seq(b)
    env.insert_scheme(
        "builtin-map".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string(), "b".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (
                        None,
                        Type::Function {
                            params: vec![(None, Type::TypeVar("a".to_string(), 0))],
                            ret: Box::new(Type::TypeVar("b".to_string(), 0)),
                            variadic: false,
                            required_count: 1,
                        },
                    ),
                    (None, tycon_app("Seq", Type::TypeVar("a".to_string(), 0))),
                ],
                ret: Box::new(tycon_app("Seq", Type::TypeVar("b".to_string(), 0))),
                variadic: false,
                required_count: 2,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-filter: ∀a. (a → Bool) → Seq(a) → Seq(a)
    env.insert_scheme(
        "builtin-filter".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (
                        None,
                        Type::Function {
                            params: vec![(None, Type::TypeVar("a".to_string(), 0))],
                            ret: Box::new(Type::Int),
                            variadic: false,
                            required_count: 1,
                        },
                    ),
                    (None, tycon_app("Seq", Type::TypeVar("a".to_string(), 0))),
                ],
                ret: Box::new(tycon_app("Seq", Type::TypeVar("a".to_string(), 0))),
                variadic: false,
                required_count: 2,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-take: ∀T. Int → Seq(T) → Seq(T)
    env.insert_scheme(
        "builtin-take".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (None, Type::Int),
                    (None, tycon_app("Seq", Type::TypeVar("T".to_string(), 0))),
                ],
                ret: Box::new(tycon_app("Seq", Type::TypeVar("T".to_string(), 0))),
                variadic: false,
                required_count: 2,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-drop: ∀T. Int → Seq(T) → Seq(T)
    env.insert_scheme(
        "builtin-drop".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (None, Type::Int),
                    (None, tycon_app("Seq", Type::TypeVar("T".to_string(), 0))),
                ],
                ret: Box::new(tycon_app("Seq", Type::TypeVar("T".to_string(), 0))),
                variadic: false,
                required_count: 2,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // ── Sequences: reductions ─────────────────────────────────────────────────
    // builtin-reduce: ∀a b. (b → a → b) → b → Seq(a) → b
    // The standard left fold: (accumulator → element → accumulator) → initial → seq → result.
    env.insert_scheme(
        "builtin-reduce".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string(), "b".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (
                        None,
                        Type::Function {
                            params: vec![
                                (None, Type::TypeVar("b".to_string(), 0)),
                                (None, Type::TypeVar("a".to_string(), 0)),
                            ],
                            ret: Box::new(Type::TypeVar("b".to_string(), 0)),
                            variadic: false,
                            required_count: 2,
                        },
                    ),
                    (None, Type::TypeVar("b".to_string(), 0)),
                    (None, tycon_app("Seq", Type::TypeVar("a".to_string(), 0))),
                ],
                ret: Box::new(Type::TypeVar("b".to_string(), 0)),
                variadic: false,
                required_count: 3,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    env.insert(
        "builtin-join".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Str),
                // Top: join stringifies any element type via stringify().
                (None, tycon_app("Seq", Type::Any)),
            ],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-concat: Concatable a b c => a -> b -> c
    // The FD (a,b)→c allows the type checker to infer the result type precisely:
    // Seq(T)++Seq(T)→Seq(T), Record++Record→Record, Str++Str→Str, Bytes++Bytes→Bytes.
    env.insert_scheme(
        "builtin-concat".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            constraints: vec![Constraint::Class {
                class: Arc::clone(&concatable_class),
                vars: vec![
                    ConstraintArg::Var("a".to_string()),
                    ConstraintArg::Var("b".to_string()),
                    ConstraintArg::Var("c".to_string()),
                ],
                origin_name: None,
                origin_span: None,
            }],
            body: Type::Function {
                params: vec![
                    (None, Type::TypeVar("a".to_string(), 0)),
                    (None, Type::TypeVar("b".to_string(), 0)),
                ],
                ret: Box::new(Type::TypeVar("c".to_string(), 0)),
                variadic: false,
                required_count: 2,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // ── Sequences: list operations ────────────────────────────────────────────
    // builtin-rest: ∀T. (Seq(T) | Record({})) → (Seq(T) | Record({}))
    // The implementation (in builtins.rs builtin_rest) dispatches on Seq vs Dict inputs.
    // Both Seq(T) and open Dict (Record({})) are valid inputs and outputs.
    env.insert_scheme(
        "builtin-rest".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![(
                    None,
                    Type::Union(vec![
                        tycon_app("Seq", Type::TypeVar("T".to_string(), 0)),
                        Type::Record(Row {
                            fields: indexmap::IndexMap::new(),
                            tail: crate::type_def::RowTail::Empty,
                        }),
                    ]),
                )],
                ret: Box::new(Type::Union(vec![
                    tycon_app("Seq", Type::TypeVar("T".to_string(), 0)),
                    Type::Record(Row {
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ])),
                variadic: false,
                required_count: 1,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-reverse: {} -> {}
    // Takes a materialized integer-keyed Dict and returns elements in reverse insertion order.
    // Callers must collect any lazy Seq first (via tinct collect) before calling this.
    // Reversing a lazy Seq is not a trivial operation and requires explicit materialization.
    env.insert(
        "builtin-reverse".to_string(),
        Type::Function {
            params: vec![(
                None,
                Type::Record(Row {
                    fields: indexmap::IndexMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }),
            )],
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-sort: ∀T. Seq(T) → Seq(T)
    // Natural ordering sort of a sequence. The 1-arg form ([builtin-sort xs]) is the public
    // API used by sorted/sorted-by in the prelude. The 2-arg comparator form ([builtin-sort cmp xs])
    // is called from sort-by and is handled at runtime via Rust's builtin_sort variadic dispatch;
    // that call site has its own return annotation (@[Seq a]) in the prelude.
    env.insert_scheme(
        "builtin-sort".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![(None, tycon_app("Seq", Type::TypeVar("T".to_string(), 0)))],
                ret: Box::new(tycon_app("Seq", Type::TypeVar("T".to_string(), 0))),
                variadic: false,
                required_count: 1,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // proxy: (Str → Top) → Proxy
    env.insert(
        "proxy".to_string(),
        Type::Function {
            params: vec![(
                None,
                Type::Function {
                    params: vec![(None, Type::Str)],
                    ret: Box::new(Type::Any),
                    variadic: false,
                    required_count: 1,
                },
            )],
            ret: Box::new(Type::Proxy),
            variadic: false,
            required_count: 1,
        },
    );

    // ── builtin-get / get: Indexable c k v => k -> c -> v ────────────────────
    // T-1104 NOTE: The canonical names `builtin-get`, and its prelude re-export `get` MUST
    // stay in core_type_env because they are used by the degraded scheme restoration loop.
    // The prelude wrappers carry the Indexable constraint, but SCC-interaction issues in the
    // constraint generalization machinery cause the FD to fail at call sites. The authoritative
    // builtin scheme ensures `get 1 (Seq[String])` resolves the return type to `String` via
    // Indexable FD machinery. Without these registrations, the restoration loop would find
    // nothing to restore, breaking Indexable FD improvement. See B-384 fix.
    //
    // NOTE: `get?` is a tinct-level function in prelude that composes `builtin-has-key?` +
    // `builtin-get` + `Absent.Absent`. It also needs a dual-registered Indexable scheme so that
    // the FD machinery resolves the element type from the container type at `get?` call sites
    // (same pattern as `get`).
    for get_name in ["builtin-get", "get"] {
        env.insert_scheme(
            get_name.to_string(),
            TypeScheme {
                type_vars: vec!["c".to_string(), "k".to_string(), "v".to_string()],
                constraints: vec![Constraint::Class {
                    class: Arc::clone(&indexable_class),
                    vars: vec![
                        ConstraintArg::Var("c".to_string()),
                        ConstraintArg::Var("k".to_string()),
                        ConstraintArg::Var("v".to_string()),
                    ],
                    origin_name: None,
                    origin_span: None,
                }],
                body: Type::Function {
                    params: vec![
                        (None, Type::TypeVar("k".to_string(), 0)),
                        (None, Type::TypeVar("c".to_string(), 0)),
                    ],
                    ret: Box::new(Type::TypeVar("v".to_string(), 0)),
                    variadic: false,
                    required_count: 2,
                },
                label_vars: vec![],
                kind_vars: Vec::new(),
                doc: None,
                inner_schemes: None,
            },
        );
    }

    // builtin-has-key?: (Int | Str) → Dict → Int
    // Returns Int 1 if key exists, Int 0 if absent. O(1) spine-only check.
    // Used by prelude to implement get? without Rust knowing about the Absent type.
    env.insert(
        "builtin-has-key?".to_string(),
        Type::Function {
            params: vec![
                (None, Type::normalize_union(vec![Type::Int, Type::Str])),
                (None, Type::Any),
            ],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );

    // get?: Indexable c k v => k → c → v | Absent
    // Registered in core_type_env for FD machinery (same pattern as builtin-get/get).
    // The runtime implementation is a tinct function in prelude that composes
    // builtin-has-key? + builtin-get + Absent.Absent.
    env.insert_scheme(
        "get?".to_string(),
        TypeScheme {
            type_vars: vec!["c".to_string(), "k".to_string(), "v".to_string()],
            constraints: vec![Constraint::Class {
                class: Arc::clone(&indexable_class),
                vars: vec![
                    ConstraintArg::Var("c".to_string()),
                    ConstraintArg::Var("k".to_string()),
                    ConstraintArg::Var("v".to_string()),
                ],
                origin_name: None,
                origin_span: None,
            }],
            body: Type::Function {
                params: vec![
                    (None, Type::TypeVar("k".to_string(), 0)),
                    (None, Type::TypeVar("c".to_string(), 0)),
                ],
                ret: Box::new(Type::normalize_union(vec![
                    Type::TypeVar("v".to_string(), 0),
                    Type::Record(Row {
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ])),
                variadic: false,
                required_count: 2,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // builtin-get-by-field: String → Any → Dict → Any (T-1378)
    // Reverse lookup on a type-level lookup table: given a field name, a field value,
    // and a type constructor dict, returns the first variant whose compile-time constant
    // for that field equals the target value, or errors if no match.
    env.insert(
        "builtin-get-by-field".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 3,
        },
    );

    // ── builtin-first / builtin-last ──────────────────────────────────────────
    // builtin-first: ∀T. Seq(T) → T
    // Returns the first element of a sequence. The previous ∀a. a→a (identity) was wrong:
    // it accepts any value and promises to return the same type, hiding the Seq requirement.
    env.insert_scheme(
        "builtin-first".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![(None, tycon_app("Seq", Type::TypeVar("T".to_string(), 0)))],
                ret: Box::new(Type::TypeVar("T".to_string(), 0)),
                variadic: false,
                required_count: 1,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-last: ∀T. Seq(T) → T
    // Returns the last element of a sequence. Same fix as builtin-first.
    env.insert_scheme(
        "builtin-last".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![(None, tycon_app("Seq", Type::TypeVar("T".to_string(), 0)))],
                ret: Box::new(Type::TypeVar("T".to_string(), 0)),
                variadic: false,
                required_count: 1,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // ── Async concurrency ─────────────────────────────────────────────────────
    // Task and Channel are opaque runtime types; use Top for all async I/O
    // boundaries. The key requirement is that every entry has type Function (not
    // Unknown or Error) so prelude wrappers (await, task, …) resolve correctly.

    // builtin-task: Top → Top
    // Takes any expression value (the task body evaluated lazily), returns an opaque Task.
    // The runtime does not require a zero-arg function — any value is accepted.
    env.insert(
        "builtin-task".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // Task — opaque async handle
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-await: Top → Top
    // Awaits a Task and returns its result.
    env.insert(
        "builtin-await".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // Task result — genuinely opaque
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-channel: Int → Top
    // Creates a buffered channel with the given capacity.
    env.insert(
        "builtin-channel".to_string(),
        Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Any), // Channel — opaque
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-send: Top → Top → Top
    // Sends a value on a channel: (channel, value) → Null/result
    env.insert(
        "builtin-send".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-recv: Top → Top
    // Receives a value from a channel.
    env.insert(
        "builtin-recv".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // Received value — genuinely opaque
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-select-once: Top → Top → Top
    // Selects the first ready channel from a seq of SelectSource values.
    // First arg is a context (for cancellation), second arg is the sources Seq.
    env.insert(
        "builtin-select-once".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-broadcast-channel: Int → Top
    // Creates a broadcast channel of the given capacity.
    // Each subscriber (via recv) receives every value sent after it subscribes.
    env.insert(
        "builtin-broadcast-channel".to_string(),
        Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Any), // BroadcastChannel — opaque
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-oneshot-channel: () → Top
    // Creates a one-shot channel (single send/recv pair).
    // Returns a dict with sender and receiver fields.
    env.insert(
        "builtin-oneshot-channel".to_string(),
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Any), // {sender, receiver} dict — opaque
            variadic: false,
            required_count: 0,
        },
    );
    // builtin-try-send: Top → Top → Top
    // Non-blocking send: returns [Ok null] if sent, [Full] if buffer is full.
    env.insert(
        "builtin-try-send".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-par: Top → Top → Top
    // Runs two thunks in parallel, returns both results.
    env.insert(
        "builtin-par".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-par-map: ∀a b. (a → b) → Seq(a) → Seq(b)
    env.insert_scheme(
        "builtin-par-map".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string(), "b".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (
                        None,
                        Type::Function {
                            params: vec![(None, Type::TypeVar("a".to_string(), 0))],
                            ret: Box::new(Type::TypeVar("b".to_string(), 0)),
                            variadic: false,
                            required_count: 1,
                        },
                    ),
                    (None, tycon_app("Seq", Type::TypeVar("a".to_string(), 0))),
                ],
                ret: Box::new(tycon_app("Seq", Type::TypeVar("b".to_string(), 0))),
                variadic: false,
                required_count: 2,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-par-filter: ∀a. (a → Bool) → Seq(a) → Seq(a)
    env.insert_scheme(
        "builtin-par-filter".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (
                        None,
                        Type::Function {
                            params: vec![(None, Type::TypeVar("a".to_string(), 0))],
                            ret: Box::new(Type::Int),
                            variadic: false,
                            required_count: 1,
                        },
                    ),
                    (None, tycon_app("Seq", Type::TypeVar("a".to_string(), 0))),
                ],
                ret: Box::new(tycon_app("Seq", Type::TypeVar("a".to_string(), 0))),
                variadic: false,
                required_count: 2,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-signal-channel: Top → Top
    // Creates a channel for OS signals; argument is a seq of Signal values.
    env.insert(
        "builtin-signal-channel".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // Channel — opaque
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-timer-channel: ClockCap → (Duration | Int) → Channel (opaque)
    // Creates a timer channel; takes ClockCap and interval (Duration preferred, bare Int in milliseconds for backward compat).
    // Returns Channel@Timestamp — opaque (no Channel type variant in the type system).
    env.insert(
        "builtin-timer-channel".to_string(),
        Type::Function {
            params: vec![
                (None, Type::ClockCap),
                (None, Type::normalize_union(vec![Type::Duration, Type::Int])),
            ],
            ret: Box::new(Type::Any), // Channel — opaque, no Channel type variant
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-watch-channel: DirCap → String → Channel (opaque)
    // Filesystem watch channel — sends null when the file/directory at path changes.
    // Returns Channel@Null — opaque (no Channel type variant in the type system).
    env.insert(
        "builtin-watch-channel".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::Any), // Channel@Null — opaque, no Channel type variant
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-context: Top → Top
    // Returns the current task context. Typically called as (builtin-context).
    // Typed as 1-arg to satisfy Function requirement; arg is variadic-style optional.
    env.insert(
        "builtin-context".to_string(),
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Any), // Context — opaque
            variadic: true,
            required_count: 0,
        },
    );
    // builtin-with-cancel: Top → Top
    // Creates a cancellable child context from a parent context.
    env.insert(
        "builtin-with-cancel".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // (Context, cancel-fn) pair — opaque
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-with-timeout: ClockCap → Top → Duration → Top
    // Creates a context with a timeout; (ClockCap, parent-context, duration) → Context
    env.insert(
        "builtin-with-timeout".to_string(),
        Type::Function {
            params: vec![
                (None, Type::ClockCap),
                (None, Type::Any),
                (None, Type::Duration),
            ],
            ret: Box::new(Type::Any), // Context — opaque
            variadic: false,
            required_count: 3,
        },
    );
    // builtin-with-deadline: ClockCap → Top → Timestamp → Top
    // Creates a context with a deadline; (ClockCap, parent-context, deadline) → Context
    env.insert(
        "builtin-with-deadline".to_string(),
        Type::Function {
            params: vec![
                (None, Type::ClockCap),
                (None, Type::Any),
                (None, Type::Timestamp),
            ],
            ret: Box::new(Type::Any), // Context — opaque
            variadic: false,
            required_count: 3,
        },
    );
    // builtin-cancelled?: Top → Bool
    // Checks whether a context has been cancelled.
    env.insert(
        "builtin-cancelled-q".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-cancel-task: Top → Top
    // Cancels a task or context.
    env.insert(
        "builtin-cancel-task".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-non-cancellable: Top → Top
    // Wraps a thunk so it runs in a non-cancellable context.
    env.insert(
        "builtin-non-cancellable".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-with-context: Top → Top → Top
    // Runs a thunk with a specific context: (context, thunk) → result
    env.insert(
        "builtin-with-context".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-cancel-root: Top → Top
    // Cancels the root context of the current task.
    env.insert(
        "builtin-cancel-root".to_string(),
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Any),
            variadic: true,
            required_count: 0,
        },
    );
    // builtin-drain: Top → Top
    // Drains a channel, consuming all buffered values.
    env.insert(
        "builtin-drain".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-exit-now: Int → Unknown
    // Immediately terminates the process with the given exit code.
    env.insert(
        "builtin-exit-now".to_string(),
        Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Never), // process terminates — logically Never
            variadic: false,
            required_count: 1,
        },
    );

    // ── Type introspection ────────────────────────────────────────────────────
    // builtin-ast-of: Unknown → Unknown
    // Takes any expression unevaluated (Strictness::Id), returns a metadata Dict
    // describing the AST or runtime thunk state. Shape is entirely runtime-dependent.
    // Public alias "ast-of" is re-exported from prelude.llt.
    env.insert(
        "builtin-ast-of".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)], // any expression — not materialized
            ret: Box::new(Type::Any),        // metadata Dict — shape runtime-dependent
            variadic: false,
            required_count: 1,
        },
    );

    // builtin-sequential: Dict → Expression  (int-keyed dict of Expression nodes → Sequential)
    // Used by boot-level macros to construct a Sequential AST node before the prelude's
    // Expr type is in scope. Returns Expression (typed as Any — runtime shape only).
    env.insert(
        "builtin-sequential".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // returns Expression (typed as Top for now)
            variadic: false,
            required_count: 1,
        },
    );

    // builtin-ast-to-program: Expression → Program (requires call-site-span: named arg)
    // Converts an Expr.* AST node to a Value::Program wrapper.
    env.insert(
        "builtin-ast-to-program".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // returns Program (typed as Top for now)
            variadic: false,
            required_count: 1,
        },
    );

    // ── Type constructors ─────────────────────────────────────────────────────
    // Map with Unknown K/V is the unparameterized Map type.
    env.insert("Map".to_string(), Type::map(Type::Unknown, Type::Unknown));
    // Map[K V] as a parameterized type alias.
    env.insert_tycon_def(
        "Map".to_string(),
        Arc::new(TyConDef {
            params: vec!["k".to_string(), "v".to_string()],
            body: Type::map(
                Type::TypeVar("k".to_string(), 0),
                Type::TypeVar("v".to_string(), 0),
            ),
            constraints: vec![],
            variance: vec![],
            constructors: vec![],
            builtin_type: None,
            annotation: None,
            field_annotations: indexmap::IndexMap::new(),
            constructor_constants: indexmap::IndexMap::new(),
        }),
    );

    // ── Capability and handle type aliases ────────────────────────────────────
    // Register as type aliases so @DirCap, @NetCap, @File are valid in user annotations.
    // @Handle removed: Value::Handle no longer exists.
    env.insert_tycon_def(
        "DirCap".to_string(),
        Arc::new(TyConDef {
            params: vec![],
            body: Type::DirCap,
            constraints: vec![],
            variance: vec![],
            constructors: vec![],
            builtin_type: None,
            annotation: None,
            field_annotations: indexmap::IndexMap::new(),
            constructor_constants: indexmap::IndexMap::new(),
        }),
    );
    env.insert_tycon_def(
        "NetCap".to_string(),
        Arc::new(TyConDef {
            params: vec![],
            body: Type::NetCap,
            constraints: vec![],
            variance: vec![],
            constructors: vec![],
            builtin_type: None,
            annotation: None,
            field_annotations: indexmap::IndexMap::new(),
            constructor_constants: indexmap::IndexMap::new(),
        }),
    );
    // File: raw OS file primitive (Value::File). No type parameters — opaque handle.
    env.insert_tycon_def(
        "File".to_string(),
        Arc::new(TyConDef {
            params: vec![],
            body: Type::Unknown,
            constraints: vec![],
            variance: vec![],
            constructors: vec![],
            builtin_type: None,
            annotation: None,
            field_annotations: indexmap::IndexMap::new(),
            constructor_constants: indexmap::IndexMap::new(),
        }),
    );

    // ── DirCap capability flags ───────────────────────────────────────────────
    // Singleton unit types for DirCap permission markers.
    // Used in intersection types to express fine-grained capabilities.
    for flag_name in [
        "Readable",
        "Writable",
        "Listable",
        "Statable",
        "Appendable",
        "Deletable",
        "Renameable",
    ] {
        let mut fields = indexmap::IndexMap::new();
        fields.insert(
            format!("__cap_flag_{}", flag_name.to_lowercase()),
            Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            }),
        );
        env.insert_tycon_def(
            flag_name.to_string(),
            Arc::new(TyConDef {
                params: vec![],
                body: Type::Record(Row {
                    fields,
                    tail: crate::type_def::RowTail::Empty,
                }),
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: None,
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
            }),
        );
    }

    // ── Handle mode flags ─────────────────────────────────────────────────────
    // Singleton unit types for I/O handle capability markers.
    for flag_name in [
        "Binary",
        "Seekable",
        "Stream",
        "Tls",
        "Text",
        "Exclusive",
        "Sync",
        "NoFollow",
    ] {
        let mut fields = indexmap::IndexMap::new();
        fields.insert(
            format!("__cap_flag_{}", flag_name.to_lowercase()),
            Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            }),
        );
        env.insert_tycon_def(
            flag_name.to_string(),
            Arc::new(TyConDef {
                params: vec![],
                body: Type::Record(Row {
                    fields,
                    tail: crate::type_def::RowTail::Empty,
                }),
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: None,
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
            }),
        );
    }

    // ── HashAlgorithm type alias ──────────────────────────────────────────────
    // Union of string literal types for hash algorithm identifiers.
    // Used as the algorithm argument to hash and SPKI pin functions.
    env.insert_tycon_def(
        "HashAlgorithm".to_string(),
        Arc::new(TyConDef {
            params: vec![],
            body: Type::normalize_union(vec![
                Type::StringLiteral("Sha256".to_string()),
                Type::StringLiteral("Sha384".to_string()),
                Type::StringLiteral("Sha512".to_string()),
                Type::StringLiteral("Sha3-256".to_string()),
                Type::StringLiteral("Sha3-384".to_string()),
                Type::StringLiteral("Sha3-512".to_string()),
                Type::StringLiteral("Blake3".to_string()),
            ]),
            constraints: vec![],
            variance: vec![],
            constructors: vec![],
            builtin_type: None,
            annotation: None,
            field_annotations: indexmap::IndexMap::new(),
            constructor_constants: indexmap::IndexMap::new(),
        }),
    );

    // ── builtin-* aliases for core operators ─────────────────────────────────
    // When a document declares `--- uses: ["core"]`, `type_env_module("core")`
    // injects this env. User code may then use `builtin-add`, `builtin-mul`, etc.
    // directly (bypassing prelude). These aliases must mirror the canonical forms
    // already registered above so that the type checker accepts them.
    //
    // T-1104: Many canonical names were removed from core_type_env (now provided by
    // prelude wrappers). The corresponding builtin-* alias entries were also removed
    // from this loop. Only aliases whose canonical is still registered above remain.
    //
    // Note: aliases that already have their own direct registrations above
    // (e.g. `builtin-get`, `builtin-take`) are NOT included here — the loop would
    // overwrite them with the canonical form's TypeScheme instead of their own
    // more-precise scheme.
    for (alias, canonical) in [
        // Arithmetic operators
        ("builtin-add", "+"),
        ("builtin-sub", "-"),
        ("builtin-mul", "*"),
        ("builtin-div", "/"),
        // Comparison operators
        ("builtin-lt", "<"),
        ("builtin-gt", ">"),
        ("builtin-gte", ">="),
        ("builtin-lte", "<="),
        // String operations (int->string and float->string retained as primitive printers)
        ("builtin-int->string", "int->string"),
        ("builtin-float->string", "float->string"),
        // Numeric math (still registered above)
        ("builtin-pow", "pow"),
        ("builtin-sqrt", "sqrt"),
        ("builtin-log", "log"),
        ("builtin-log2", "log2"),
        ("builtin-log10", "log10"),
        ("builtin-exp", "exp"),
        ("builtin-sin", "sin"),
        ("builtin-cos", "cos"),
        ("builtin-tan", "tan"),
        ("builtin-asin", "asin"),
        ("builtin-acos", "acos"),
        ("builtin-atan", "atan"),
        ("builtin-atan2", "atan2"),
        ("builtin-nan?", "nan?"),
        ("builtin-inf?", "inf?"),
        ("builtin-finite?", "finite?"),
        ("builtin-shl", "shl"),
        ("builtin-shr", "shr"),
        ("builtin-float", "float"),
        // Meta (bare-name forms still registered above)
        ("builtin-to-tinct", "to-tinct"),
        ("builtin-annotation-of", "annotation-of"),
        ("builtin-make-annotated", "make-annotated"),
    ] {
        if let Some(scheme) = env.get(canonical).cloned() {
            env.insert_scheme(alias.to_string(), scheme);
        }
    }

    // Equirecursive type system builtins — used internally by the type checker
    // and exposed for testing/meta-programming. Return type is Bool or Unknown.
    // builtin-is-contractive: Top → Bool
    env.insert(
        "builtin-is-contractive".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 1,
        },
    );

    // ── Missing builtin-* type entries ────────────────────────────────────────
    // The entries below were registered in core_builtins() but absent from this
    // function, causing prelude functions that call them to receive <error> type,
    // cascading to make those prelude functions invisible to user code.
    //
    // The alias loop at the bottom of this function maps e.g. "builtin-add" → "+"
    // only if "+" is already in env.  But "+", "-", "*", "/", "=", "<", ">", "<=",
    // ">=" come from prelude typeclass instances — they are never in the static
    // core_type_env.  So arithmetic and comparison builtin-* names must be inserted
    // directly.  All I/O, builder, string-ext, meta, and reactive-cell entries have
    // the same problem.

    // ── Arithmetic ────────────────────────────────────────────────────────────
    // builtin-add / builtin-sub / builtin-mul / builtin-div: Top → Top → Top
    // Using Top inputs: instance bodies annotate args with specific types (@Int, @Float)
    // which provide precision via FD inference. Using Number would reject String args
    // in user-defined instances and break type checking of annotated instance bodies.
    for name in ["builtin-add", "builtin-sub", "builtin-mul", "builtin-div"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Any), (None, Type::Any)],
                ret: Box::new(Type::Any),
                variadic: false,
                required_count: 2,
            },
        );
    }

    // ── Comparison ────────────────────────────────────────────────────────────
    // All comparison builtins: Top → Top → Bool
    // Using Top inputs: builtin-lt is called with String, Bool args by Comparable instances.
    // Number would incorrectly reject those calls during prelude type-checking.
    for name in ["builtin-lt", "builtin-gt", "builtin-lte", "builtin-gte"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Any), (None, Type::Any)],
                ret: Box::new(Type::Int),
                variadic: false,
                required_count: 2,
            },
        );
    }

    // Type-specific equality primitives — used by Equatable instances.
    // builtin-eq-int: Int → Int → Int
    env.insert(
        "builtin-eq-int".to_string(),
        Type::Function {
            params: vec![(None, Type::Int), (None, Type::Int)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-eq-float: Float → Float → Int
    env.insert(
        "builtin-eq-float".to_string(),
        Type::Function {
            params: vec![(None, Type::Float), (None, Type::Float)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-eq-string: Str → Str → Int
    env.insert(
        "builtin-eq-string".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Str)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );

    // ── Numeric rounding / parsing ────────────────────────────────────────────
    // builtin-floor / builtin-round: Number → Int
    for name in ["builtin-floor", "builtin-round"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::normalize_union(vec![Type::Int, Type::Float]))],
                ret: Box::new(Type::Int),
                variadic: false,
                required_count: 1,
            },
        );
    }
    // builtin-to-int: Top → Int  (parse string or truncate float)
    env.insert(
        "builtin-to-int".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Int),
            variadic: true, // accepts optional named args
            required_count: 1,
        },
    );
    // builtin-to-float: Top → Float  (parse string or convert int)
    env.insert(
        "builtin-to-float".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Float),
            variadic: false,
            required_count: 1,
        },
    );

    // ── Bitwise ───────────────────────────────────────────────────────────────
    // builtin-band / builtin-bor / builtin-bxor: Int → Int → Int
    for name in ["builtin-band", "builtin-bor", "builtin-bxor"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Int), (None, Type::Int)],
                ret: Box::new(Type::Int),
                variadic: false,
                required_count: 2,
            },
        );
    }

    // ── String operations ─────────────────────────────────────────────────────
    // builtin-replace: Str → Str → Str → Str  (pattern, replacement, input)
    env.insert(
        "builtin-replace".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Str), (None, Type::Str)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 3,
        },
    );
    // builtin-trim / builtin-trim-start / builtin-trim-end: Str → Str
    for name in ["builtin-trim", "builtin-trim-start", "builtin-trim-end"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Str),
                variadic: false,
                required_count: 1,
            },
        );
    }
    // builtin-str-length: Str → Int
    env.insert(
        "builtin-str-length".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-str-byte-count: Str → Int  (UTF-8 byte length, O(1))
    env.insert(
        "builtin-str-byte-count".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-str-has-nth-byte?: Str → Int → Int  (1 if index valid, 0 otherwise)
    env.insert(
        "builtin-str-has-nth-byte?".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Int)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-str-nth-byte: Str → Int → Int  (UTF-8 byte value 0-255)
    env.insert(
        "builtin-str-nth-byte".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Int)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-str-slice: Str → Int → Int → Str  (string, start, end)
    env.insert(
        "builtin-str-slice".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Int), (None, Type::Int)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 3,
        },
    );
    // builtin-str-has-nth?: Str → Int → Int  (1 if index i exists, 0 if OOB)
    env.insert(
        "builtin-str-has-nth?".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Int)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-str-nth-char: Str → Int → Str  (single char at index i; errors on OOB)
    env.insert(
        "builtin-str-nth-char".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Int)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-char-code: Str → Int  (Unicode codepoint of first char)
    env.insert(
        "builtin-char-code".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-chr: Int → Str  (Unicode codepoint to single-char string)
    env.insert(
        "builtin-chr".to_string(),
        Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-str-bytes: Str → Bytes  (UTF-8 encoding)
    env.insert(
        "builtin-str-bytes".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Bytes),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-bytes-str: Bytes → Str  (UTF-8 decode)
    env.insert(
        "builtin-bytes-str".to_string(),
        Type::Function {
            params: vec![(None, Type::Bytes)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-str-index-of: Str → Str → Int  (needle, haystack → index or -1)
    env.insert(
        "builtin-str-index-of".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Str)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-str-to-upper-char / builtin-str-to-lower-char: Str → Str
    for name in ["builtin-str-to-upper-char", "builtin-str-to-lower-char"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Str),
                variadic: false,
                required_count: 1,
            },
        );
    }
    // builtin-str-map-chars: (Str → Str) → Str → Str
    env.insert(
        "builtin-str-map-chars".to_string(),
        Type::Function {
            params: vec![
                (
                    None,
                    Type::Function {
                        params: vec![(None, Type::Str)],
                        ret: Box::new(Type::Str),
                        variadic: false,
                        required_count: 1,
                    },
                ),
                (None, Type::Str),
            ],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-regex-match?: Str → Str → Bool  (pattern, input)
    env.insert(
        "builtin-regex-match?".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Str)],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );

    // ── Builder operations ─────────────────────────────────────────────────────
    // All builder ops use Top for the builder value (opaque runtime type).
    // builtin-make-builder: () → Top  (create a new mutable builder)
    env.insert(
        "builtin-make-builder".to_string(),
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Any),
            variadic: true,
            required_count: 0,
        },
    );
    // builtin-builder-set: Top → (Int|Str) → Top → Top  (builder, key, value → builder)
    env.insert(
        "builtin-builder-set".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Any),
                (None, Type::normalize_union(vec![Type::Int, Type::Str])),
                (None, Type::Any),
            ],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 3,
        },
    );
    // builtin-builder-delete: Top → (Int|Str) → Top  (builder, key → builder)
    env.insert(
        "builtin-builder-delete".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Any),
                (None, Type::normalize_union(vec![Type::Int, Type::Str])),
            ],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-builder-finish: Top → Record({})  (builder → immutable dict)
    env.insert(
        "builtin-builder-finish".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-builder-snapshot: Top → Record({})  (non-consuming snapshot)
    env.insert(
        "builtin-builder-snapshot".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-builder-has?: Top → (Int|Str) → Bool
    env.insert(
        "builtin-builder-has?".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Any),
                (None, Type::normalize_union(vec![Type::Int, Type::Str])),
            ],
            ret: Box::new(Type::Int),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-builder-get: Top → (Int|Str) → Top
    env.insert(
        "builtin-builder-get".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Any),
                (None, Type::normalize_union(vec![Type::Int, Type::Str])),
            ],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-builder-get-or: Top → Top → (Int|Str) → Top  (builder, default, key)
    env.insert(
        "builtin-builder-get-or".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Any),
                (None, Type::Any),
                (None, Type::normalize_union(vec![Type::Int, Type::Str])),
            ],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 3,
        },
    );

    // ── I/O — missing entries ─────────────────────────────────────────────────
    // Null type helper reused across multiple I/O return types.
    let null_record = Type::Record(Row {
        fields: indexmap::IndexMap::new(),
        tail: crate::type_def::RowTail::Empty,
    });

    // builtin-open removed (operated on Value::Handle). Use builtin-file-open instead.
    // builtin-stat: DirCap → Str → {name: Str, kind: Str, size: Int, ...}
    env.insert(
        "builtin-stat".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::Record(Row {
                fields: indexmap::IndexMap::from_iter([
                    ("name".to_string(), Type::Str),
                    ("kind".to_string(), Type::Str),
                    ("size".to_string(), Type::Int),
                ]),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-list-dir: DirCap → Str → Seq({name: Str, kind: Str})
    env.insert(
        "builtin-list-dir".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(tycon_app(
                "Seq",
                Type::Record(Row {
                    fields: indexmap::IndexMap::from_iter([
                        ("name".to_string(), Type::Str),
                        ("kind".to_string(), Type::Str),
                    ]),
                    tail: crate::type_def::RowTail::Empty,
                }),
            )),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-write: DirCap → Str → (Str | Bytes) → Null
    env.insert(
        "builtin-write".to_string(),
        Type::Function {
            params: vec![
                (None, Type::DirCap),
                (None, Type::Str),
                (None, Type::normalize_union(vec![Type::Str, Type::Bytes])),
            ],
            ret: Box::new(null_record.clone()),
            variadic: false,
            required_count: 3,
        },
    );
    // builtin-write-atomic: DirCap → Str → (Str | Bytes) → Null
    env.insert(
        "builtin-write-atomic".to_string(),
        Type::Function {
            params: vec![
                (None, Type::DirCap),
                (None, Type::Str),
                (None, Type::normalize_union(vec![Type::Str, Type::Bytes])),
            ],
            ret: Box::new(null_record.clone()),
            variadic: false,
            required_count: 3,
        },
    );
    // builtin-write-handle, builtin-flush, builtin-close, builtin-string-handle
    // type env entries removed (operated on Value::Handle/WriteHandle which no longer exist).
    // builtin-make-dir: DirCap → Str → Null
    env.insert(
        "builtin-make-dir".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(null_record.clone()),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-rename: DirCap → Str → Str → Null
    env.insert(
        "builtin-rename".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
            ret: Box::new(null_record.clone()),
            variadic: false,
            required_count: 3,
        },
    );
    // builtin-link: DirCap → Str → Str → Null  (target, link-cap, link-path)
    env.insert(
        "builtin-link".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
            ret: Box::new(null_record.clone()),
            variadic: false,
            required_count: 3,
        },
    );
    // builtin-read-link: DirCap → Str → Str
    env.insert(
        "builtin-read-link".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-path-dir: Str → DirCap  (parent directory of a file path, scoped to base_dir)
    env.insert(
        "builtin-path-dir".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::DirCap),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-revoke-cap: Top → Null  (revoke a RevocableDirCap)
    env.insert(
        "builtin-revoke-cap".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(null_record.clone()),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-cap-data, builtin-raw-create, builtin-seek, builtin-seek-end, builtin-position
    // type env entries removed (operated on Value::Handle which no longer exists).

    // ── Meta / reflection — missing entries ───────────────────────────────────
    // builtin-span-of: Top → Top  (extract span metadata from a value)
    env.insert(
        "builtin-span-of".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // Span dict — runtime-determined shape
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-gensym: Str → Str → Str  (scope, prefix → unique identifier)
    env.insert(
        "builtin-gensym".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Str)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 2,
        },
    );
    // builtin-llt-repr: Top → Str  (produce tinct source repr of a value)
    env.insert(
        "builtin-llt-repr".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Str),
            variadic: false,
            required_count: 1,
        },
    );
    // Program and Document are runtime-v2 AST value types (Value::Program, Value::Document).
    // They are typed as open record types encoding their known dot-accessible fields.
    // This allows the type checker to verify patterns like `prog.documents`,
    // `doc.expressions`, etc. without requiring dedicated Type::Program/Type::Document
    // variants (Sprint 1, Part F). Unknown fields resolve to Top via the open row tail.
    //
    // Expression: open record representing Value::Expression AST nodes (T-1272).
    //
    // The Fn constructor's `return-ann: Annotation` field is the load-bearing registration
    // that resolves T013 ambiguity warnings in generate.llt. Without this registration,
    // dot-access on an Expression value falls to the `_` arm of check_dot_access and
    // fails with NotARecord, causing downstream T013 ambiguity on Indexable constraints.
    //
    // `return-ann` holds whatever annotation type the prelude defines. We use Type::Any
    // to avoid coupling the Rust type env to a prelude-level type name ("Annotation").
    // Pattern match narrowing (`[match ann [Annotation.PropertyDict p]]`) still works
    // via TyCon expansion in typecheck.rs when Annotation is in state.tycon_env — the
    // field type being Any is gradual: Any is consistent with any annotation type.
    //
    // The open row tail (Uniform { value: Any }) allows access to any other field on
    // Expression values (e.g., `span`, `fn`, `body`) without a static error.
    let mut expr_fields: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
    expr_fields.insert(
        "return-ann".to_string(),
        Type::Any, // Annotation type from prelude — Any avoids coupling to a prelude type name
    );
    expr_fields.insert(
        "params".to_string(),
        tycon_app("Seq", Type::Any), // Seq[Parameter] — Parameter is Top for now
    );
    expr_fields.insert("span".to_string(), Type::Any); // Span — open row tail covers this too
    let expression_type = Type::Record(Row {
        fields: expr_fields,
        tail: crate::type_def::RowTail::Uniform {
            key: None,
            value: Box::new(Type::Any),
        },
    });

    // Document: open record with expressions, name, stage, uses fields
    let mut doc_fields: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
    doc_fields.insert(
        "expressions".to_string(),
        tycon_app("Seq", expression_type.clone()), // Seq[Expression] — Expression open record (T-1272)
    );
    doc_fields.insert("name".to_string(), Type::Any); // Named/Unnamed variant
    doc_fields.insert("stage".to_string(), Type::Any); // DocStage.Type / DocStage.Runtime
    doc_fields.insert("uses".to_string(), tycon_app("Seq", Type::Str)); // Seq[String] module names
    let document_type = Type::Record(Row {
        fields: doc_fields,
        tail: crate::type_def::RowTail::Uniform {
            key: None,
            value: Box::new(Type::Any),
        },
    });

    // Program: open record with documents field (Seq[Document])
    let mut prog_fields: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
    prog_fields.insert(
        "documents".to_string(),
        tycon_app("Seq", document_type.clone()),
    );
    let program_type = Type::Record(Row {
        fields: prog_fields,
        tail: crate::type_def::RowTail::Uniform {
            key: None,
            value: Box::new(Type::Any),
        },
    });

    // builtin-load: Str → Program  (parse source text into a Program value)
    // Optional named args: name: Str (display path), hash: Str (blake3 integrity digest).
    env.insert(
        "builtin-load".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Str),                     // positional: source text
                (Some("name".to_string()), Type::Str), // named, optional: display path
                (Some("hash".to_string()), Type::Str), // named, optional: blake3 hex digest
            ],
            ret: Box::new(program_type.clone()),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-program: Seq[Document] → Program  (construct a Program from documents)
    env.insert(
        "builtin-program".to_string(),
        Type::Function {
            params: vec![(None, tycon_app("Seq", document_type))],
            ret: Box::new(program_type),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-eval-types: Top → Top  (type-check a Program value and return type info)
    env.insert(
        "builtin-eval-types".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // Type info dict — runtime-determined shape
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-decimal: Top → Decimal  (construct exact decimal from string or number)
    env.insert(
        "builtin-decimal".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // Decimal — opaque numeric type
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-big-int: Top → BigInt  (construct arbitrary-precision integer)
    env.insert(
        "builtin-big-int".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // BigInt — opaque numeric type
            variadic: false,
            required_count: 1,
        },
    );

    // ── Reactive cells ────────────────────────────────────────────────────────
    // builtin-reactive-cell: Top → Top  (create a reactive cell with initial value)
    env.insert(
        "builtin-reactive-cell".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any), // ReactiveCell — opaque
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-cell-get: Top → Top  (read current value from a reactive cell)
    env.insert(
        "builtin-cell-get".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 1,
        },
    );
    // builtin-cell-set: Top → Top → Top  (cell, new-value → Null or cell)
    env.insert(
        "builtin-cell-set".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 2,
        },
    );

    // ── Control flow — bare `if` and `until` ─────────────────────────────────
    // `if` is registered in core_builtins() as an alias for builtin-if.
    // It must have a type entry so prelude code that calls `if` directly type-checks.
    env.insert(
        "if".to_string(),
        Type::Function {
            params: vec![(None, Type::Int), (None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 3,
        },
    );
    // `builtin-until` is registered in core_builtins() for iterative loops.
    // Top → Top is the safe approximation (takes a thunk, returns its result).
    env.insert(
        "builtin-until".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: true,
            required_count: 1,
        },
    );

    // ── Capability stubs — injected by the loader at runtime ───────────────────
    // %prelude is the prelude environment injected by the loader before prelude loads.
    // The type checker must see it as a valid name to avoid "undefined variable" warnings
    // in prelude code that references it.
    env.insert("%prelude".to_string(), Type::Any);

    // ── Missing builtins — registered in core_builtins but not previously in type env ──
    // These all use Any for gradual typing; they exist at runtime and the type checker
    // must be able to see them as callable names.
    for name in [
        "builtin-dict-has-nth?",
        "builtin-dict-has-key-nth?",
        "builtin-dict-has-kv-nth?",
        "builtin-file-open",
        "builtin-file-close",
        "builtin-file-read",
        "builtin-file-write",
        "builtin-file-seek",
        "builtin-file-flush",
        "builtin-extend-env",
        "builtin-eval-macro-ast",
    ] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Any)],
                ret: Box::new(Type::Any),
                variadic: true,
                required_count: 0,
            },
        );
    }

    // ── Operator stubs — used in prelude but dispatched via typeclass instances ───
    // The type checker processes prelude dicts sequentially and cannot see typeclass-
    // dispatched operators (=, <, etc.) that are defined in later dicts. Adding stubs
    // here lets the type checker see them as callable without false "undefined variable"
    // errors. The runtime always uses the correct typeclass-dispatched implementations.
    for name in ["=", "!=", "<", ">", "<=", ">="] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Any), (None, Type::Any)],
                ret: Box::new(Type::Any),
                variadic: false,
                required_count: 2,
            },
        );
    }

    // ── builtin-proxy ─────────────────────────────────────────────────────────
    // builtin-proxy: (Str → Top) → Proxy
    // Registered in core_builtins() alongside bare "proxy" (which has its own entry above).
    env.insert(
        "builtin-proxy".to_string(),
        Type::Function {
            params: vec![(
                None,
                Type::Function {
                    params: vec![(None, Type::Str)],
                    ret: Box::new(Type::Any),
                    variadic: false,
                    required_count: 1,
                },
            )],
            ret: Box::new(Type::Proxy),
            variadic: false,
            required_count: 1,
        },
    );

    // ── get-in: path-following field access ──────────────────────────────────
    // get-in: Seq(Str) → Any → Unknown
    // The return type is path-dependent and cannot be statically determined from
    // this simple type signature. The sync infer_surface_expr provides special-case
    // handling that infers precise types for literal string paths.
    // Registration here ensures "get-in" is defined so type-checking does not
    // report it as undefined.
    env.insert(
        "get-in".to_string(),
        Type::Function {
            params: vec![(None, Type::Any), (None, Type::Any)],
            ret: Box::new(Type::Unknown),
            variadic: false,
            required_count: 2,
        },
    );
}
