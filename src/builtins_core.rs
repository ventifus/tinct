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
    builtin_bor, builtin_bxor, builtin_cos, builtin_div_float, builtin_eq, builtin_exp,
    builtin_finite_check, builtin_float, builtin_gt, builtin_gte, builtin_if, builtin_inf_check,
    builtin_log, builtin_log10, builtin_log2, builtin_lt, builtin_lte, builtin_mul,
    builtin_nan_check, builtin_pow, builtin_shl, builtin_shr, builtin_sin, builtin_sqrt,
    builtin_sub, builtin_tan,
};
// Dict/access implementations.
use crate::builtins_dict::{
    builtin_append, builtin_build_dict, builtin_builder_delete, builtin_builder_finish,
    builtin_builder_get, builtin_builder_get_or, builtin_builder_has, builtin_builder_set,
    builtin_builder_snapshot, builtin_each, builtin_each_key, builtin_each_kv, builtin_get,
    builtin_get_optional, builtin_keys, builtin_length, builtin_make_builder, builtin_merge,
};
// String implementations.
use crate::builtins_string::{
    builtin_bytes_str, builtin_char_code, builtin_chr, builtin_regex_match, builtin_replace,
    builtin_split, builtin_str, builtin_str_bytes, builtin_str_chars, builtin_str_index_of,
    builtin_str_length, builtin_str_map_chars, builtin_str_slice, builtin_str_to_lower_char,
    builtin_str_to_upper_char, builtin_trim, builtin_trim_end, builtin_trim_start,
};
// Bytes implementations.
use crate::builtins_bytes::{
    builtin_bytes, builtin_bytes_equal, builtin_bytes_find, builtin_bytes_of, builtin_ct_equal,
};
// Numeric (floor, round) and parsing (to-int, to-float) implementations — live in builtins.rs.
use crate::builtins::{builtin_floor, builtin_round, builtin_to_float, builtin_to_int};
// Stream output implementations.
use crate::stream::builtin_to_tinct;
// Meta/eval implementations.
use crate::builtins_meta::{
    builtin_annotation_of, builtin_apply, builtin_ast_of, builtin_big_int, builtin_blake3,
    builtin_builtin_module, builtin_cap_identity, builtin_decimal, builtin_eval,
    builtin_eval_types, builtin_expand, builtin_force, builtin_gensym, builtin_include_cache_get,
    builtin_include_cache_put, builtin_is_contractive, builtin_llt_repr, builtin_load,
    builtin_macro_error, builtin_macro_injects, builtin_make_annotated, builtin_program,
    builtin_raise, builtin_tag_of, builtin_try, builtin_type_of, builtin_until, builtin_validate,
    builtin_variant,
};
// I/O implementations.
use crate::builtins_io::{
    builtin_cap_data, builtin_close, builtin_copy_file, builtin_emit, builtin_env, builtin_exists,
    builtin_flush, builtin_get_xattr, builtin_link, builtin_list_dir, builtin_list_xattrs,
    builtin_make_dir, builtin_narrow, builtin_open, builtin_position, builtin_raw_create,
    builtin_read_all, builtin_read_chunk, builtin_read_line, builtin_read_link, builtin_remove,
    builtin_remove_xattr, builtin_rename, builtin_revocable, builtin_revoke_cap, builtin_seek,
    builtin_seek_end, builtin_set_permissions, builtin_set_xattr, builtin_stat,
    builtin_stat_symlink, builtin_string_handle, builtin_symlink, builtin_write,
    builtin_write_atomic, builtin_write_handle,
};
// Sequence primitive implementations.
use crate::builtins_seq_prim::{builtin_collect, builtin_head, builtin_seq, builtin_tail};
// Sequence generator implementations.
use crate::builtins_seq_gen::{
    builtin_cycle, builtin_iterate, builtin_range, builtin_repeat, builtin_unfold,
};
// Sequence transform implementations.
use crate::builtins_seq_xform::{builtin_drop, builtin_filter, builtin_map, builtin_take};
// Sequence reduction implementations.
use crate::builtins_seq_reduce::{builtin_concat, builtin_join, builtin_reduce};
// List operation implementations — live in builtins.rs.
use crate::builtins::{
    builtin_cons, builtin_first, builtin_last, builtin_proxy, builtin_rest, builtin_reverse,
    builtin_sort,
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
use crate::types::{ClassDecl, Constraint, Kind, Row, Type, TypeAlias, TypeEnv, TypeScheme};
use std::collections::BTreeMap;
use std::sync::Arc;

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
pub fn core_builtins() -> Vec<BuiltinDef> {
    vec![
        // ── Arithmetic ────────────────────────────────────────────────────────────────
        builtin!("+", builtin_add, [Strictness::Seq, Strictness::Seq], 2),
        builtin!("-", builtin_sub, [Strictness::Seq, Strictness::Seq], 2),
        builtin!("*", builtin_mul, [Strictness::Seq, Strictness::Seq], 2),
        builtin!(
            "/",
            builtin_div_float,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        // Stable aliases (used internally by prelude to allow user shadowing of operator names)
        builtin!(
            "builtin-add",
            builtin_add,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-sub",
            builtin_sub,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-mul",
            builtin_mul,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-div",
            builtin_div_float,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        // ── Comparison ───────────────────────────────────────────────────────────────
        builtin!("=", builtin_eq, [Strictness::Seq, Strictness::Seq], 2),
        builtin!("<", builtin_lt, [Strictness::Seq, Strictness::Seq], 2),
        // Stable aliases
        builtin!(
            "builtin-eq",
            builtin_eq,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-lt",
            builtin_lt,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-gt",
            builtin_gt,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-lte",
            builtin_lte,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-gte",
            builtin_gte,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        // ── Control flow ─────────────────────────────────────────────────────────────
        builtin!(
            "if",
            builtin_if,
            [Strictness::Seq, Strictness::Id, Strictness::Id],
            1
        ),
        builtin!(
            "builtin-if",
            builtin_if,
            [Strictness::Seq, Strictness::Id, Strictness::Id],
            1
        ),
        // ── Dict primitives ──────────────────────────────────────────────────────────
        builtin!("builtin-keys", builtin_keys, [Strictness::Spine], 1),
        builtin!("builtin-length", builtin_length, [Strictness::Spine], 1),
        builtin!("builtin-merge", builtin_merge),
        builtin!(
            "builtin-append",
            builtin_append,
            [Strictness::Id, Strictness::Seq],
            0
        ),
        builtin!(
            "builtin-get",
            builtin_get,
            [Strictness::Seq, Strictness::Spine],
            2
        ),
        builtin!(
            "get?",
            builtin_get_optional,
            [Strictness::Seq, Strictness::Spine],
            2
        ),
        builtin!(
            "builtin-each",
            builtin_each,
            [Strictness::Spine, Strictness::Spine]
        ),
        builtin!(
            "builtin-each-key",
            builtin_each_key,
            [Strictness::Spine, Strictness::Spine]
        ),
        builtin!(
            "builtin-each-kv",
            builtin_each_kv,
            [Strictness::Spine, Strictness::Spine]
        ),
        builtin!(
            "builtin-build-dict",
            builtin_build_dict,
            [Strictness::Spine],
            1
        ),
        // ── Builder ops ──────────────────────────────────────────────────────────────
        builtin!("builtin-make-builder", builtin_make_builder),
        builtin!(
            "builtin-builder-set",
            builtin_builder_set,
            [Strictness::Seq, Strictness::Id, Strictness::Seq],
            0
        ),
        builtin!(
            "builtin-builder-delete",
            builtin_builder_delete,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-builder-finish",
            builtin_builder_finish,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "builtin-builder-snapshot",
            builtin_builder_snapshot,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "builtin-builder-has?",
            builtin_builder_has,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-builder-get",
            builtin_builder_get,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-builder-get-or",
            builtin_builder_get_or,
            [Strictness::Seq, Strictness::Id, Strictness::Seq],
            0
        ),
        // ── String ops ───────────────────────────────────────────────────────────────
        builtin!("builtin-str", builtin_str, [Strictness::Seq]),
        builtin!(
            "builtin-split",
            builtin_split,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-replace",
            builtin_replace,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3
        ),
        builtin!("builtin-trim", builtin_trim, [Strictness::Seq], 1),
        builtin!(
            "builtin-str-length",
            builtin_str_length,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "builtin-str-slice",
            builtin_str_slice,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3
        ),
        builtin!("builtin-str-chars", builtin_str_chars, [Strictness::Seq], 1),
        builtin!("builtin-char-code", builtin_char_code, [Strictness::Seq], 1),
        builtin!("builtin-chr", builtin_chr, [Strictness::Seq], 1),
        builtin!("builtin-str-bytes", builtin_str_bytes, [Strictness::Seq], 1),
        builtin!("builtin-bytes-str", builtin_bytes_str, [Strictness::Seq], 1),
        builtin!(
            "builtin-str-index-of",
            builtin_str_index_of,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-trim-start",
            builtin_trim_start,
            [Strictness::Seq],
            1
        ),
        builtin!("builtin-trim-end", builtin_trim_end, [Strictness::Seq], 1),
        builtin!(
            "builtin-str-to-upper-char",
            builtin_str_to_upper_char,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "builtin-str-to-lower-char",
            builtin_str_to_lower_char,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "builtin-str-map-chars",
            builtin_str_map_chars,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-regex-match?",
            builtin_regex_match,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        // ── Bytes ────────────────────────────────────────────────────────────────────
        builtin!("bytes", builtin_bytes, []),
        builtin!(
            "bytes-find",
            builtin_bytes_find,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!("bytes-of", builtin_bytes_of, [Strictness::Seq]),
        builtin!(
            "bytes-equal?",
            builtin_bytes_equal,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "ct-equal?",
            builtin_ct_equal,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        // ── Math ─────────────────────────────────────────────────────────────────────
        builtin!("builtin-floor", builtin_floor, [Strictness::Seq], 1),
        builtin!("builtin-round", builtin_round, [Strictness::Seq], 1),
        builtin!(
            "builtin-pow",
            builtin_pow,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!("builtin-sqrt", builtin_sqrt, [Strictness::Seq], 1),
        builtin!("builtin-log", builtin_log, [Strictness::Seq], 1),
        builtin!("builtin-log2", builtin_log2, [Strictness::Seq], 1),
        builtin!("builtin-log10", builtin_log10, [Strictness::Seq], 1),
        builtin!("builtin-exp", builtin_exp, [Strictness::Seq], 1),
        builtin!("builtin-sin", builtin_sin, [Strictness::Seq], 1),
        builtin!("builtin-cos", builtin_cos, [Strictness::Seq], 1),
        builtin!("builtin-tan", builtin_tan, [Strictness::Seq], 1),
        builtin!("builtin-asin", builtin_asin, [Strictness::Seq], 1),
        builtin!("builtin-acos", builtin_acos, [Strictness::Seq], 1),
        builtin!("builtin-atan", builtin_atan, [Strictness::Seq], 1),
        builtin!(
            "builtin-atan2",
            builtin_atan2,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!("builtin-nan?", builtin_nan_check, [Strictness::Seq], 1),
        builtin!("builtin-inf?", builtin_inf_check, [Strictness::Seq], 1),
        builtin!(
            "builtin-finite?",
            builtin_finite_check,
            [Strictness::Seq],
            1
        ),
        // ── Bitwise ──────────────────────────────────────────────────────────────────
        builtin!(
            "builtin-band",
            builtin_band,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-bor",
            builtin_bor,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-bxor",
            builtin_bxor,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-shl",
            builtin_shl,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-shr",
            builtin_shr,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        // ── Type conversion ──────────────────────────────────────────────────────────
        builtin!("builtin-float", builtin_float, [Strictness::Seq], 1),
        builtin!("builtin-to-int", builtin_to_int, [Strictness::Seq]),
        builtin!("builtin-to-float", builtin_to_float, [Strictness::Seq]),
        // ── Evaluation control ───────────────────────────────────────────────────────
        builtin!("materialize", builtin_force, [Strictness::Seq]),
        builtin!("builtin-raise", builtin_raise, [Strictness::Seq]),
        builtin!(
            "builtin-macro-error",
            builtin_macro_error,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!("builtin-try", builtin_try, [Strictness::Id], 1),
        builtin!(
            "builtin-apply",
            builtin_apply,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!("until", builtin_until),
        // ── Type introspection ───────────────────────────────────────────────────────
        builtin!("builtin-type-of", builtin_type_of, [Strictness::Seq]),
        builtin!("builtin-ast-of", builtin_ast_of, [Strictness::Id]),
        // ── Schema validation ────────────────────────────────────────────────────────
        builtin!(
            "validate",
            builtin_validate,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        // ── I/O ──────────────────────────────────────────────────────────────────────
        builtin!("builtin-emit", builtin_emit, [Strictness::Seq]),
        builtin!("builtin-env", builtin_env, [Strictness::Seq]),
        builtin!(
            "builtin-open",
            builtin_open,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "builtin-narrow",
            builtin_narrow,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("builtin-revocable", builtin_revocable, [Strictness::Seq]),
        builtin!("builtin-revoke-cap", builtin_revoke_cap, [Strictness::Seq]),
        builtin!(
            "builtin-string-handle",
            builtin_string_handle,
            [Strictness::Seq]
        ),
        builtin!("builtin-read-line", builtin_read_line, [Strictness::Seq]),
        builtin!(
            "builtin-read-chunk",
            builtin_read_chunk,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-write",
            builtin_write,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3
        ),
        builtin!(
            "builtin-write-atomic",
            builtin_write_atomic,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3
        ),
        builtin!(
            "builtin-cap-data",
            builtin_cap_data,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-write-handle",
            builtin_write_handle,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!("builtin-flush", builtin_flush, [Strictness::Seq]),
        builtin!("builtin-close", builtin_close, [Strictness::Seq]),
        builtin!(
            "builtin-raw-create",
            builtin_raw_create,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-seek",
            builtin_seek,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!("builtin-seek-end", builtin_seek_end, [Strictness::Seq]),
        builtin!("builtin-position", builtin_position, [Strictness::Seq]),
        builtin!(
            "builtin-list-dir",
            builtin_list_dir,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-stat",
            builtin_stat,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-exists",
            builtin_exists,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-stat-symlink",
            builtin_stat_symlink,
            [Strictness::Seq, Strictness::Seq],
            2
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
            4
        ),
        builtin!(
            "builtin-symlink",
            builtin_symlink,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3
        ),
        builtin!(
            "builtin-set-permissions",
            builtin_set_permissions,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3
        ),
        builtin!(
            "builtin-get-xattr",
            builtin_get_xattr,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3
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
            4
        ),
        builtin!(
            "builtin-remove-xattr",
            builtin_remove_xattr,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3
        ),
        builtin!(
            "builtin-list-xattrs",
            builtin_list_xattrs,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-make-dir",
            builtin_make_dir,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-remove",
            builtin_remove,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!(
            "builtin-rename",
            builtin_rename,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3
        ),
        builtin!(
            "builtin-link",
            builtin_link,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3
        ),
        builtin!(
            "builtin-read-link",
            builtin_read_link,
            [Strictness::Seq, Strictness::Seq],
            2
        ),
        builtin!("builtin-read-all", builtin_read_all, [Strictness::Seq]),
        // ── Decomposed include primitives ─────────────────────────────────────────────
        builtin!("builtin-blake3", builtin_blake3, [Strictness::Seq]),
        builtin!(
            "builtin-cap-identity",
            builtin_cap_identity,
            [Strictness::Seq]
        ),
        builtin!("builtin-expand", builtin_expand, [Strictness::Seq], 1),
        builtin!("builtin-load", builtin_load, [Strictness::Seq], 1),
        builtin!("builtin-program", builtin_program, [Strictness::Spine], 1),
        builtin!(
            "builtin-module",
            builtin_builtin_module,
            [Strictness::Seq],
            1
        ),
        builtin!("builtin-eval", builtin_eval, [Strictness::Seq], 1),
        builtin!(
            "builtin-eval-types",
            builtin_eval_types,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "builtin-include-cache-get",
            builtin_include_cache_get,
            [Strictness::Seq],
            1
        ),
        builtin!(
            "builtin-include-cache-put",
            builtin_include_cache_put,
            [],
            2
        ),
        // ── Sequences — primitives ────────────────────────────────────────────────────
        builtin!("builtin-seq", builtin_seq),
        builtin!("builtin-head", builtin_head, [Strictness::Seq]),
        builtin!("builtin-tail", builtin_tail, [Strictness::Seq]),
        builtin!("builtin-collect", builtin_collect, [Strictness::Spine]),
        builtin!(
            "builtin-range",
            builtin_range,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("builtin-repeat", builtin_repeat),
        builtin!("builtin-cycle", builtin_cycle, [Strictness::Spine], 1),
        builtin!("builtin-iterate", builtin_iterate),
        builtin!("builtin-unfold", builtin_unfold),
        // ── Sequences — transforms ────────────────────────────────────────────────────
        builtin!(
            "builtin-map",
            builtin_map,
            [Strictness::Id, Strictness::Spine],
            1
        ),
        builtin!(
            "builtin-filter",
            builtin_filter,
            [Strictness::Id, Strictness::Spine],
            1
        ),
        builtin!(
            "builtin-take",
            builtin_take,
            [Strictness::Seq, Strictness::Spine],
            2
        ),
        builtin!(
            "builtin-drop",
            builtin_drop,
            [Strictness::Seq, Strictness::Spine],
            2
        ),
        builtin!(
            "builtin-reduce",
            builtin_reduce,
            [Strictness::Id, Strictness::Id, Strictness::Spine]
        ),
        builtin!(
            "builtin-join",
            builtin_join,
            [Strictness::Seq, Strictness::Spine],
            2
        ),
        builtin!(
            "builtin-concat",
            builtin_concat,
            [Strictness::Spine, Strictness::Seq],
            1
        ),
        // ── Sequences — list operations ───────────────────────────────────────────────
        builtin!("builtin-first", builtin_first, [Strictness::Spine]),
        builtin!("builtin-last", builtin_last, [Strictness::Spine]),
        builtin!("builtin-rest", builtin_rest, [Strictness::Spine]),
        builtin!(
            "builtin-cons",
            builtin_cons,
            [Strictness::Id, Strictness::Spine]
        ),
        builtin!("builtin-reverse", builtin_reverse, [Strictness::Spine]),
        builtin!(
            "builtin-sort",
            builtin_sort,
            [Strictness::Spine, Strictness::Spine]
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
            1
        ),
        builtin!("builtin-watch-channel", builtin_watch_channel),
        builtin!("builtin-context", builtin_context),
        builtin!(
            "builtin-with-cancel",
            builtin_with_cancel,
            [Strictness::Seq]
        ),
        builtin!(
            "builtin-with-timeout",
            builtin_with_timeout,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            1
        ),
        builtin!(
            "builtin-with-deadline",
            builtin_with_deadline,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            1
        ),
        builtin!(
            "builtin-cancelled-q",
            builtin_cancelled_q,
            [Strictness::Seq]
        ),
        builtin!(
            "builtin-cancel-task",
            builtin_cancel_task,
            [Strictness::Seq]
        ),
        builtin!("builtin-non-cancellable", builtin_non_cancellable),
        builtin!(
            "builtin-with-context",
            builtin_with_context,
            [Strictness::Seq, Strictness::Id],
            1
        ),
        builtin!("builtin-cancel-root", builtin_cancel_root),
        builtin!("builtin-drain", builtin_drain),
        builtin!("builtin-exit-now", builtin_exit_now, [Strictness::Seq]),
        // ── Reactive cells (T-831) ────────────────────────────────────────────────────
        builtin!("builtin-reactive-cell", builtin_reactive_cell),
        builtin!("builtin-cell-get", builtin_cell_get),
        builtin!("builtin-cell-set", builtin_cell_set),
        // ── Meta / reflection ─────────────────────────────────────────────────────────
        builtin!(
            "builtin-gensym",
            builtin_gensym,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("builtin-llt-repr", builtin_llt_repr, [Strictness::Seq]),
        builtin!("builtin-to-tinct", builtin_to_tinct, [Strictness::Seq], 1),
        builtin!("builtin-tag-of", builtin_tag_of, [Strictness::Seq]),
        builtin!(
            "builtin-variant",
            builtin_variant,
            [Strictness::Seq, Strictness::Id]
        ),
        builtin!(
            "builtin-annotation-of",
            builtin_annotation_of,
            [Strictness::Seq]
        ),
        builtin!(
            "builtin-make-annotated",
            builtin_make_annotated,
            [Strictness::Seq, Strictness::Seq]
        ),
        // S-861: equirecursive-checker — contractiveness check for mu combinator.
        // Used by stdlib/prelude.llt type-stage `mu` to validate TypeNode.Recursive bodies.
        // Also called from expand_named in typecheck_annot.rs (wired in S-861, both still
        // dead code pending annotation resolver wiring in S-862).
        builtin!(
            "builtin-is-contractive",
            builtin_is_contractive,
            [Strictness::Seq]
        ),
        builtin!("builtin-decimal", builtin_decimal, [Strictness::Seq]),
        builtin!("builtin-big-int", builtin_big_int, [Strictness::Seq]),
        builtin!("builtin-proxy", builtin_proxy),
        builtin!(
            "builtin-macro-injects",
            builtin_macro_injects,
            [Strictness::Seq]
        ),
    ]
}

/// Populate `env` with the type signatures for all "core" module builtins.
///
/// This is the type-system counterpart of `core_builtins()` (T-714). It contains every
/// `env.insert*` call from the former `TypeEnv::with_builtins()` (deleted in T-722) that belongs to the
/// "core" module — i.e., everything EXCEPT datetime types and net/URI types.
///
/// ## Exclusions (in their own modules — T-715/T-716 complete)
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
///
/// ## Relationship to the deleted `TypeEnv::with_builtins()`
///
/// `TypeEnv::with_builtins()` was deleted in T-722. This function contains the
/// same registrations for the core subset. Callers of the old `with_builtins()` now use
/// `crate::builtins::build_builtins_type_env()` which delegates to this function.
pub fn core_type_env(env: &mut TypeEnv) {
    // ── Type class declarations ────────────────────────────────────────────────
    // These match the declarations registered in InferState::new().

    let addable_class = Arc::new(ClassDecl {
        name: "Addable".to_string(),
        params: vec![
            ("a".to_string(), Kind::Type),
            ("b".to_string(), Kind::Type),
            ("c".to_string(), Kind::Type),
        ],
        superclasses: vec![],
        determines: vec![(vec![0, 1], vec![2])], // (a,b) → c
        resolver: None,
        resolver_injective: false,
    });

    let subtractable_class = Arc::new(ClassDecl {
        name: "Subtractable".to_string(),
        params: vec![
            ("a".to_string(), Kind::Type),
            ("b".to_string(), Kind::Type),
            ("c".to_string(), Kind::Type),
        ],
        superclasses: vec![],
        determines: vec![(vec![0, 1], vec![2])], // (a,b) → c
        resolver: None,
        resolver_injective: false,
    });

    let multipliable_class = Arc::new(ClassDecl {
        name: "Multipliable".to_string(),
        params: vec![
            ("a".to_string(), Kind::Type),
            ("b".to_string(), Kind::Type),
            ("c".to_string(), Kind::Type),
        ],
        superclasses: vec![],
        determines: vec![(vec![0, 1], vec![2])], // (a,b) → c
        resolver: None,
        resolver_injective: false,
    });

    let divisible_class = Arc::new(ClassDecl {
        name: "Divisible".to_string(),
        params: vec![
            ("a".to_string(), Kind::Type),
            ("b".to_string(), Kind::Type),
            ("c".to_string(), Kind::Type),
        ],
        superclasses: vec![],
        determines: vec![(vec![0, 1], vec![2])], // (a,b) → c
        resolver: None,
        resolver_injective: false,
    });

    let equatable_class = Arc::new(ClassDecl {
        name: "Equatable".to_string(),
        params: vec![("a".to_string(), Kind::Type)],
        superclasses: vec![],
        determines: vec![],
        resolver: None,
        resolver_injective: false,
    });

    let comparable_class = Arc::new(ClassDecl {
        name: "Comparable".to_string(),
        params: vec![("a".to_string(), Kind::Type)],
        superclasses: vec![("Equatable".to_string(), vec!["a".to_string()])],
        determines: vec![],
        resolver: None,
        resolver_injective: false,
    });

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
    });

    // ── Arithmetic ────────────────────────────────────────────────────────────
    // Addition: Addable a b c => a -> b -> c
    env.insert_scheme(
        "+".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            constraints: vec![Constraint::Class {
                class: Arc::clone(&addable_class),
                vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
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
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // Subtraction: Subtractable a b c => a -> b -> c
    env.insert_scheme(
        "-".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            constraints: vec![Constraint::Class {
                class: Arc::clone(&subtractable_class),
                vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
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
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // Multiplication: Multipliable a b c => a -> b -> c
    env.insert_scheme(
        "*".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            constraints: vec![Constraint::Class {
                class: Arc::clone(&multipliable_class),
                vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
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
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // Division: Divisible a b c => a -> b -> c
    env.insert_scheme(
        "/".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            constraints: vec![Constraint::Class {
                class: Arc::clone(&divisible_class),
                vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
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
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // Equality: Equatable a => a -> a -> Bool
    env.insert_scheme(
        "=".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string()],
            constraints: vec![Constraint::new(Arc::clone(&equatable_class), "a")],
            body: Type::Function {
                params: vec![
                    (None, Type::TypeVar("a".to_string(), 0)),
                    (None, Type::TypeVar("a".to_string(), 0)),
                ],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // Less-than: Comparable a => a -> a -> Bool
    env.insert_scheme(
        "<".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string()],
            constraints: vec![Constraint::new(Arc::clone(&comparable_class), "a")],
            body: Type::Function {
                params: vec![
                    (None, Type::TypeVar("a".to_string(), 0)),
                    (None, Type::TypeVar("a".to_string(), 0)),
                ],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // Greater-than: Comparable a => a -> a -> Bool
    env.insert_scheme(
        ">".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string()],
            constraints: vec![Constraint::new(Arc::clone(&comparable_class), "a")],
            body: Type::Function {
                params: vec![
                    (None, Type::TypeVar("a".to_string(), 0)),
                    (None, Type::TypeVar("a".to_string(), 0)),
                ],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // Greater-than-or-equal: Comparable a => a -> a -> Bool
    env.insert_scheme(
        ">=".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string()],
            constraints: vec![Constraint::new(Arc::clone(&comparable_class), "a")],
            body: Type::Function {
                params: vec![
                    (None, Type::TypeVar("a".to_string(), 0)),
                    (None, Type::TypeVar("a".to_string(), 0)),
                ],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // Less-than-or-equal: Comparable a => a -> a -> Bool
    env.insert_scheme(
        "<=".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string()],
            constraints: vec![Constraint::new(Arc::clone(&comparable_class), "a")],
            body: Type::Function {
                params: vec![
                    (None, Type::TypeVar("a".to_string(), 0)),
                    (None, Type::TypeVar("a".to_string(), 0)),
                ],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // ── Control flow ──────────────────────────────────────────────────────────
    // if takes Bool and two branches, returns Top (type depends on branches)
    env.insert(
        "if".to_string(),
        Type::Function {
            params: vec![
                (Some("condition".to_string()), Type::Bool),
                (Some("then_".to_string()), Type::Top),
                (Some("else_".to_string()), Type::Top),
            ],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );

    // ── Dict primitives ───────────────────────────────────────────────────────
    // keys: Record({}) → Seq(Int | Str)
    // Dict keys are Int (integer-indexed / Seq) or Str (string-keyed). Both cases must be
    // reflected in the return type; Seq(Str) was wrong for integer-keyed dicts.
    env.insert(
        "keys".to_string(),
        Type::Function {
            params: vec![(
                None,
                Type::Record(Row {
                    fields: BTreeMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }),
            )],
            ret: Box::new(Type::seq(Type::Union(vec![Type::Int, Type::Str]))),
            variadic: false,
        },
    );
    env.insert(
        "length".to_string(),
        Type::Function {
            // builtin_length dispatches on Value::Dict, Value::String, Value::Bytes,
            // and integer-keyed Dicts (which are represented as Seq at the type level).
            params: vec![(
                None,
                Type::Union(vec![
                    Type::Record(Row {
                        fields: BTreeMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                    Type::Str,
                    Type::Bytes,
                    Type::seq(Type::Top),
                ]),
            )],
            ret: Box::new(Type::Int),
            variadic: false,
        },
    );
    // merge: Dict → Dict → Dict
    env.insert(
        "merge".to_string(),
        Type::Function {
            params: vec![
                (
                    Some("dict1".to_string()),
                    Type::Record(Row {
                        fields: BTreeMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ),
                (
                    Some("dict2".to_string()),
                    Type::Record(Row {
                        fields: BTreeMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ),
            ],
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    env.insert(
        "append".to_string(),
        Type::Function {
            params: vec![
                (Some("value".to_string()), Type::Top),
                (
                    Some("dict".to_string()),
                    Type::Record(Row {
                        fields: BTreeMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ),
            ],
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );

    // ── Builder primitives ────────────────────────────────────────────────────
    // Transient mutable dict builders. Canonical names for inject_builtin_aliases().
    // Parameter order follows T-777 (key-first, builder-last).
    // make-builder: () → Unknown
    env.insert(
        "make-builder".to_string(),
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Unknown), // Builder — opaque mutable container
            variadic: false,
        },
    );
    // builder-set: (key, value, builder) → Top
    // T-777 parameter order: key-first, builder-last (was builder-first)
    env.insert(
        "builder-set".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Top), // key
                (None, Type::Top), // value
                (None, Type::Top), // builder
            ],
            ret: Box::new(Type::Top), // builder (mutated in-place)
            variadic: false,
        },
    );
    // builder-delete: (key, builder) → Top
    env.insert(
        "builder-delete".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Top), // key
                (None, Type::Top), // builder
            ],
            ret: Box::new(Type::Top), // builder
            variadic: false,
        },
    );
    // builder-has?: (key, builder) → Bool
    env.insert(
        "builder-has?".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Top), // key
                (None, Type::Top), // builder
            ],
            ret: Box::new(Type::Bool),
            variadic: false,
        },
    );
    // builder-get: (key, builder) → Top
    env.insert(
        "builder-get".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Top), // key
                (None, Type::Top), // builder
            ],
            ret: Box::new(Type::Top), // value — opaque
            variadic: false,
        },
    );
    // builder-get-or: (key, default, builder) → Top
    env.insert(
        "builder-get-or".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Top), // key
                (None, Type::Top), // default
                (None, Type::Top), // builder
            ],
            ret: Box::new(Type::Top), // value or default
            variadic: false,
        },
    );
    // builder-finish: (builder) → Top
    env.insert(
        "builder-finish".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)], // builder
            ret: Box::new(Type::Top),        // Dict — shape unknown statically
            variadic: false,
        },
    );
    // builder-snapshot: (builder) → Top
    env.insert(
        "builder-snapshot".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)], // builder
            ret: Box::new(Type::Top),        // Dict snapshot
            variadic: false,
        },
    );

    // ── Reactive cells (T-831) ────────────────────────────────────────────────
    // ReactiveCell is opaque — all params and return are Unknown.
    // Canonical names for inject_builtin_aliases().
    // reactive-cell: T → ReactiveCell@T
    env.insert(
        "reactive-cell".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)], // initial value
            ret: Box::new(Type::Top),        // ReactiveCell — opaque
            variadic: false,
        },
    );
    // cell-get: ReactiveCell@T → T
    env.insert(
        "cell-get".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)], // ReactiveCell
            ret: Box::new(Type::Top),        // current value — opaque
            variadic: false,
        },
    );
    // cell-set: T → ReactiveCell@T → Null
    env.insert(
        "cell-set".to_string(),
        Type::Function {
            params: vec![(None, Type::Top), (None, Type::Top)], // val, cell
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })), // Null = empty record
            variadic: false,
        },
    );

    // ── String operations ─────────────────────────────────────────────────────
    // str: Showable a => a -> Str
    env.insert_scheme(
        "str".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string()],
            constraints: vec![Constraint::new_by_name("Showable", "a")],
            body: Type::Function {
                params: vec![(None, Type::TypeVar("a".to_string(), 0))],
                ret: Box::new(Type::Str),
                variadic: true,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    env.insert(
        "split".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Str)],
            // split returns an integer-keyed Dict of Strings. Typed as Seq[Str] so
            // that `[get N [split sep s]]` returns Str via Indexable MPTC FD (Seq[T], Int → T).
            ret: Box::new(Type::seq(Type::Str)),
            variadic: false,
        },
    );
    env.insert(
        "replace".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Str), (None, Type::Str)],
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
    for name in ["trim", "trim-start", "trim-end"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
    }
    // str-to-upper-char / str-to-lower-char: String → String (single-char primitives)
    for name in ["str-to-upper-char", "str-to-lower-char"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
    }
    // str-map-chars: (String → String) → String → String
    env.insert(
        "str-map-chars".to_string(),
        Type::Function {
            params: vec![
                (
                    None,
                    Type::Function {
                        params: vec![(None, Type::Str)],
                        ret: Box::new(Type::Str),
                        variadic: false,
                    },
                ),
                (None, Type::Str),
            ],
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
    // str-index-of: String → String → Int
    env.insert(
        "str-index-of".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Str)],
            ret: Box::new(Type::Int),
            variadic: false,
        },
    );
    // regex-match?: String → String → Bool
    env.insert(
        "regex-match?".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Str)],
            ret: Box::new(Type::Bool),
            variadic: false,
        },
    );
    // builtin-json-parse: String → Top (parsed JSON value, type is dynamic)
    env.insert(
        "builtin-json-parse".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    // String operations returning Bool
    for name in ["starts-with?", "ends-with?", "str-contains?"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Str), (None, Type::Str)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
    }
    // str-chars: String → Seq[Str]
    env.insert(
        "str-chars".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::seq(Type::Str)),
            variadic: false,
        },
    );
    // char-code: String → Int
    env.insert(
        "char-code".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Int),
            variadic: false,
        },
    );
    // chr: Int → String
    env.insert(
        "chr".to_string(),
        Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
    // str-bytes: String → Bytes
    env.insert(
        "str-bytes".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Bytes),
            variadic: false,
        },
    );
    // bytes-str: Bytes → String
    env.insert(
        "bytes-str".to_string(),
        Type::Function {
            params: vec![(None, Type::Bytes)],
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
    // str-slice: Int → Int → String → String
    env.insert(
        "str-slice".to_string(),
        Type::Function {
            params: vec![(None, Type::Int), (None, Type::Int), (None, Type::Str)],
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
    // str-length: String → Int
    env.insert(
        "str-length".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Int),
            variadic: false,
        },
    );

    // ── Bytes ─────────────────────────────────────────────────────────────────
    // bytes: variadic Bytes → Bytes (concat)
    env.insert(
        "bytes".to_string(),
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Bytes),
            variadic: true,
        },
    );
    // bytes-find: Bytes → Bytes → Int
    env.insert(
        "bytes-find".to_string(),
        Type::Function {
            params: vec![(None, Type::Bytes), (None, Type::Bytes)],
            ret: Box::new(Type::Int),
            variadic: false,
        },
    );
    // bytes-of: Seq → Bytes (or Dict → Bytes)
    env.insert(
        "bytes-of".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)], // Accepts Seq or Dict
            ret: Box::new(Type::Bytes),
            variadic: false,
        },
    );
    // bytes-equal?: Bytes → Bytes → Bool
    env.insert(
        "bytes-equal?".to_string(),
        Type::Function {
            params: vec![(None, Type::Bytes), (None, Type::Bytes)],
            ret: Box::new(Type::Bool),
            variadic: false,
        },
    );
    // ct-equal?: Bytes → Bytes → Bool
    env.insert(
        "ct-equal?".to_string(),
        Type::Function {
            params: vec![(None, Type::Bytes), (None, Type::Bytes)],
            ret: Box::new(Type::Bool),
            variadic: false,
        },
    );

    // ── Numeric operations ────────────────────────────────────────────────────
    for name in ["floor", "round"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Number)],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        );
    }
    // Math functions: 1-arg (Number -> Float)
    for name in [
        "sqrt", "log", "log2", "log10", "exp", "sin", "cos", "tan", "asin", "acos", "atan",
    ] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Number)],
                ret: Box::new(Type::Float),
                variadic: false,
            },
        );
    }
    // Math functions: 2-arg (Number -> Number -> Float)
    for name in ["pow", "atan2"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Number), (None, Type::Number)],
                ret: Box::new(Type::Float),
                variadic: false,
            },
        );
    }
    // Float predicates (Float -> Bool)
    for name in ["nan?", "inf?", "finite?"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Float)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
    }
    // Bitwise operations (Int -> Int -> Int)
    for name in ["band", "bor", "bxor", "shl", "shr"] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Int), (None, Type::Int)],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        );
    }
    // Parsing
    env.insert(
        "to-int".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Int),
            variadic: false,
        },
    );
    env.insert(
        "to-float".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Float),
            variadic: false,
        },
    );
    // float: Number → Float (converts Int or Float to Float)
    env.insert(
        "float".to_string(),
        Type::Function {
            params: vec![(None, Type::Number)],
            ret: Box::new(Type::Float),
            variadic: false,
        },
    );

    // ── Evaluation control ────────────────────────────────────────────────────
    env.insert(
        "materialize".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    env.insert(
        "raise".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Never),
            variadic: false,
        },
    );
    // try: takes 1 arg — a zero-argument function. Returns Ok(v) or Error(String).
    // Return type is Top (not a structural union) to avoid triggering T004 exhaustiveness
    // checking — the runtime returns nominal Value::Variant, not a struct dict.
    env.insert(
        "try".to_string(),
        Type::Function {
            params: vec![(
                None,
                Type::Function {
                    params: vec![],
                    ret: Box::new(Type::Top),
                    variadic: false,
                },
            )],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    env.insert(
        "apply".to_string(),
        Type::Function {
            params: vec![
                (
                    None,
                    Type::Function {
                        params: vec![(None, Type::Top)],
                        ret: Box::new(Type::Top),
                        variadic: false,
                    },
                ),
                (
                    None,
                    Type::Record(Row {
                        fields: BTreeMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ),
            ],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    // Convergence loop: until(pred, f, init) applies f until pred holds
    // ∀T. (T → Bool) → (T → T) → T → T
    env.insert_scheme(
        "until".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (
                        None,
                        Type::Function {
                            params: vec![(None, Type::TypeVar("T".to_string(), 0))],
                            ret: Box::new(Type::Bool),
                            variadic: false,
                        },
                    ),
                    (
                        None,
                        Type::Function {
                            params: vec![(None, Type::TypeVar("T".to_string(), 0))],
                            ret: Box::new(Type::TypeVar("T".to_string(), 0)),
                            variadic: false,
                        },
                    ),
                    (None, Type::TypeVar("T".to_string(), 0)),
                ],
                ret: Box::new(Type::TypeVar("T".to_string(), 0)),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // ── Schema validation ────────────────────────────────────────────────────
    // validate: takes a schema dict (Record({})) and any value, returns the
    // value if valid (or raises). The return type is Top — validate is
    // identity-like but can't express dependent types (schema→value→value).
    env.insert(
        "validate".to_string(),
        Type::Function {
            params: vec![
                (
                    None,
                    Type::Record(Row {
                        fields: BTreeMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ),
                (None, Type::Top),
            ],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );

    // ── Macro support ─────────────────────────────────────────────────────────
    // builtin-macro-error: takes a span dict and a message string, raises a
    // compile-time error at the given source location. Never returns.
    env.insert(
        "builtin-macro-error".to_string(),
        Type::Function {
            params: vec![
                (
                    None,
                    Type::Record(Row {
                        fields: BTreeMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ),
                (None, Type::Str),
            ],
            ret: Box::new(Type::Never),
            variadic: false,
        },
    );
    // builtin-macro-injects: takes a macro name (any value, looked up at
    // runtime) and returns the sequence of injected values for that macro.
    env.insert(
        "builtin-macro-injects".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::seq(Type::Top)),
            variadic: false,
        },
    );

    // ── Type introspection ────────────────────────────────────────────────────
    // These accept any value (Top), return Str
    env.insert(
        "type-of".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
    env.insert(
        "to-tinct".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
    env.insert(
        "llt-repr".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
    env.insert(
        "tag-of".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
    // variant: variadic (1 arg: tag, or 2+ args: tag + payload fields)
    // Returns Top — can't express dependent types (tag determines return type).
    env.insert(
        "variant".to_string(),
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Top),
            variadic: true,
        },
    );
    // annotation-of: takes any value, returns its annotation dict (or {} if none).
    env.insert(
        "annotation-of".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    // make-annotated: wraps a value in Value::Annotated with the given annotation dict.
    // Returns Top — the annotated wrapper is transparent to the type system.
    env.insert(
        "make-annotated".to_string(),
        Type::Function {
            params: vec![(None, Type::Top), (None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    env.insert(
        "eval-ast".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    // builtin-module: returns a dict of builtins for the named module.
    env.insert(
        "builtin-module".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Top), // Returns a Dict of builtins
            variadic: false,
        },
    );
    // builtin-eval: evaluate a Document/Program/Seq of expressions with optional env/input.
    // Return type is Top — genuinely opaque (output depends on runtime values).
    env.insert(
        "builtin-eval".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: true,
        },
    );
    // eval-types: evaluate the type-stage of a Document/Program.
    env.insert(
        "eval-types".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: true,
        },
    );
    // builtin-load: parse tinct source text into a Program value.
    // Accepts optional named args: name: String, hash: String (via variadic).
    env.insert(
        "builtin-load".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Top), // Returns a Program (AST value)
            variadic: true,
        },
    );
    // builtin-expand: macro-expand and desugar a Program value.
    env.insert(
        "builtin-expand".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top), // Returns expanded Program
            variadic: false,
        },
    );
    // builtin-blake3: compute blake3 hash of a string. Returns a hex string.
    env.insert(
        "builtin-blake3".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
    // builtin-cap-identity: return a stable string identity for a DirCap (for cache keys).
    env.insert(
        "builtin-cap-identity".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap)],
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
    // builtin-include-cache-get: look up a content-addressed include result.
    env.insert(
        "builtin-include-cache-get".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    // builtin-include-cache-put: store/update a content-addressed include result.
    env.insert(
        "builtin-include-cache-put".to_string(),
        Type::Function {
            params: vec![(None, Type::Str), (None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    env.insert(
        "gensym".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::Str),
            variadic: true, // 0 or 1 args (optional prefix)
        },
    );
    env.insert(
        "decimal".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            // Genuinely unknown: Returns Decimal type (not yet in Type enum)
            ret: Box::new(Type::Unknown),
            variadic: false,
        },
    );
    env.insert(
        "big-int".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            // Genuinely unknown: Returns BigInt type (not yet in Type enum)
            ret: Box::new(Type::Unknown),
            variadic: false,
        },
    );

    // ── Type predicates ───────────────────────────────────────────────────────
    // Accept any value (Top), return Bool
    for name in [
        "int?", "float?", "num?", "str?", "bool?", "bytes?", "null?", "dict?", "fn?", "record?",
        "map?", "seq?",
    ] {
        env.insert(
            name.to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
    }

    // ── I/O ───────────────────────────────────────────────────────────────────
    // Helper: create Handle capability flag type (Readable, Writable, etc.)
    fn cap_flag(flag_name: &str) -> Type {
        let mut fields = BTreeMap::new();
        fields.insert(
            format!("__cap_flag_{}", flag_name.to_lowercase()),
            Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            }),
        );
        Type::Record(Row {
            fields,
            tail: crate::type_def::RowTail::Empty,
        })
    }

    env.insert(
        "emit".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            // Null — Type::Record(Row::Empty), see doc/whatif/null-semantics.md
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    env.insert(
        "env".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            // Returns Str when set; Null (empty dict) when unset, --no-env active, or not in allowlist
            ret: Box::new(Type::normalize_union(vec![
                Type::Str,
                Type::Record(Row {
                    fields: BTreeMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }),
            ])),
            variadic: false,
        },
    );
    // `open` is SPECIAL-CASED in the type checker (typecheck.rs `check_open`).
    // Fallback: Handle(Unknown) for cases where flag names are not statically known.
    env.insert(
        "open".to_string(),
        Type::Function {
            params: vec![
                (None, Type::DirCap),
                (None, Type::Str),
                (None, Type::seq(Type::Top)),
            ],
            ret: Box::new(Type::handle(Type::Unknown)),
            variadic: true,
        },
    );
    env.insert(
        "string-handle".to_string(),
        Type::Function {
            params: vec![(None, Type::Str)],
            ret: Box::new(Type::handle(cap_flag("readable"))),
            variadic: false,
        },
    );
    env.insert(
        "builtin-read-line".to_string(),
        Type::Function {
            params: vec![(None, Type::handle(cap_flag("readable")))],
            // Returns Str on success, [] (null) on EOF
            ret: Box::new(Type::Union(vec![
                Type::Str,
                Type::Record(Row {
                    fields: BTreeMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }),
            ])),
            variadic: false,
        },
    );
    env.insert(
        "builtin-read-chunk".to_string(),
        Type::Function {
            params: vec![
                (None, Type::handle(cap_flag("readable"))),
                (None, Type::Int),
            ],
            // Returns Bytes on success, [] (null) on EOF
            ret: Box::new(Type::Union(vec![
                Type::Bytes,
                Type::Record(Row {
                    fields: BTreeMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }),
            ])),
            variadic: false,
        },
    );
    env.insert(
        "builtin-read-all".to_string(),
        Type::Function {
            params: vec![(None, Type::handle(cap_flag("readable")))],
            // Returns String: reads all bytes to EOF as UTF-8 text
            ret: Box::new(Type::Str),
            variadic: false,
        },
    );
    env.insert(
        "narrow".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::DirCap),
            variadic: false,
        },
    );
    env.insert(
        "write".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    env.insert(
        "write-atomic".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    env.insert(
        "revocable".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap)],
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::from([
                    ("cap".to_string(), Type::DirCap),
                    (
                        "revoke".to_string(),
                        Type::Function {
                            params: vec![],
                            ret: Box::new(Type::Record(Row {
                                fields: BTreeMap::new(),
                                tail: crate::type_def::RowTail::Empty,
                            })), // Null
                            variadic: false,
                        },
                    ),
                ]),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    env.insert(
        "revoke-cap".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap)],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    env.insert(
        "cap-data".to_string(),
        Type::Function {
            params: vec![
                // Accepts any handle — cap-data extracts metadata, not I/O operation
                (None, Type::handle(Type::Unknown)),
                (None, Type::Str),
            ],
            // Genuinely unknown: cap-data returns the value stored in the Handle's
            // capability map, which can be any type (cap name is a dynamic string key).
            ret: Box::new(Type::Unknown),
            variadic: false,
        },
    );
    env.insert(
        "write-handle".to_string(),
        Type::Function {
            params: vec![
                (None, Type::handle(cap_flag("writable"))),
                (None, Type::normalize_union(vec![Type::Str, Type::Bytes])),
            ],
            ret: Box::new(Type::handle(cap_flag("writable"))),
            variadic: false,
        },
    );
    env.insert(
        "flush".to_string(),
        Type::Function {
            // Use Top so any concrete Handle type satisfies it (is_subtype(Cap, Top) = true).
            // Unknown would fail for handles with explicit capability rows because
            // is_subtype(Cap, Unknown) = false (Unknown is not a supertype in BAS).
            params: vec![(None, Type::handle(Type::Top))],
            ret: Box::new(Type::handle(Type::Top)),
            variadic: false,
        },
    );
    env.insert(
        "close".to_string(),
        Type::Function {
            // Accepts any handle regardless of capabilities (close is always valid)
            params: vec![(None, Type::handle(Type::Unknown))],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    env.insert(
        "raw-create".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            // raw-create always creates a write-only handle
            ret: Box::new(Type::handle(cap_flag("writable"))),
            variadic: false,
        },
    );
    env.insert(
        "seek".to_string(),
        Type::Function {
            params: vec![(None, Type::handle(Type::Unknown)), (None, Type::Int)],
            // Legitimately Unknown: seek preserves input handle's capabilities.
            // Without dependent types (Handle[C] → Handle[C]), Unknown is the closest
            // approximation.
            ret: Box::new(Type::handle(Type::Unknown)),
            variadic: false,
        },
    );
    env.insert(
        "seek-end".to_string(),
        Type::Function {
            params: vec![(None, Type::handle(Type::Unknown))],
            // Legitimately Unknown: seek-end preserves input handle's capabilities.
            ret: Box::new(Type::handle(Type::Unknown)),
            variadic: false,
        },
    );
    env.insert(
        "position".to_string(),
        Type::Function {
            // Accepts any handle (position query doesn't require specific capabilities)
            params: vec![(None, Type::handle(Type::Unknown))],
            ret: Box::new(Type::Int),
            variadic: false,
        },
    );
    env.insert(
        "list-dir".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::seq(Type::Record(Row {
                fields: BTreeMap::from([
                    ("name".to_string(), Type::Str),
                    ("kind".to_string(), Type::Str),
                    ("size".to_string(), Type::Int),
                ]),
                tail: crate::type_def::RowTail::Empty,
            }))),
            variadic: false,
        },
    );
    env.insert(
        "stat".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::from([
                    ("name".to_string(), Type::Str),
                    ("kind".to_string(), Type::Str),
                    ("size".to_string(), Type::Int),
                ]),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    // builtin-exists: check whether a path exists under a capability.
    env.insert(
        "builtin-exists".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::Bool),
            variadic: false,
        },
    );
    // builtin-stat-symlink: stat a path without following symlinks.
    // Returns the same shape as stat.
    env.insert(
        "builtin-stat-symlink".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::from([
                    ("name".to_string(), Type::Str),
                    ("kind".to_string(), Type::Str),
                    ("size".to_string(), Type::Int),
                ]),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
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
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
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
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
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
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
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
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
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
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    // builtin-list-xattrs: list all extended attribute names at cap/path.
    // Returns a sequence of attribute name strings.
    env.insert(
        "builtin-list-xattrs".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::seq(Type::Str)),
            variadic: false,
        },
    );
    env.insert(
        "make-dir".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    env.insert(
        "builtin-remove".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    env.insert(
        "rename".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    env.insert(
        "link".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
            // Null — Type::Record(Row::Empty)
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    env.insert(
        "read-link".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::Str), // Returns target path as String
            variadic: false,
        },
    );
    env.insert(
        "from-json".to_string(),
        Type::Function {
            params: vec![(
                None,
                Type::Union(vec![Type::Str, Type::handle(cap_flag("readable"))]),
            )],
            // Top: JSON parse output can be any JSON value (object, array,
            // string, number, bool, null). A precise type requires schema information.
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );

    // ── Sequences: primitives ─────────────────────────────────────────────────
    // builtin-seq: ∀T. T → Seq(T) → Seq(T)  (cons-cell constructor: head, tail → Seq)
    env.insert_scheme(
        "builtin-seq".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (None, Type::TypeVar("T".to_string(), 0)),
                    (None, Type::seq(Type::TypeVar("T".to_string(), 0))),
                ],
                ret: Box::new(Type::seq(Type::TypeVar("T".to_string(), 0))),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-head: ∀T. Seq(T) → T
    env.insert_scheme(
        "builtin-head".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![(None, Type::seq(Type::TypeVar("T".to_string(), 0)))],
                ret: Box::new(Type::TypeVar("T".to_string(), 0)),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-tail: ∀T. Seq(T) → Seq(T)
    env.insert_scheme(
        "builtin-tail".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![(None, Type::seq(Type::TypeVar("T".to_string(), 0)))],
                ret: Box::new(Type::seq(Type::TypeVar("T".to_string(), 0))),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-collect: ∀T. Seq(T) → Seq(T)
    // Materializes a lazy Seq to an integer-keyed Dict, which is Seq(T) by tinct convention.
    // The previous Record({}) return was wrong — the output is indexable as Seq(T).
    env.insert_scheme(
        "builtin-collect".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![(None, Type::seq(Type::TypeVar("T".to_string(), 0)))],
                ret: Box::new(Type::seq(Type::TypeVar("T".to_string(), 0))),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-build-dict: Top → Record({})
    env.insert(
        "builtin-build-dict".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
        },
    );
    env.insert(
        "seq?".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Bool),
            variadic: false,
        },
    );

    // ── Sequences: generators ─────────────────────────────────────────────────
    // builtin-range: 1-arg (start; infinite) or 2-arg (start, end; half-open [start,end)).
    env.insert(
        "builtin-range".to_string(),
        Type::Function {
            params: vec![
                (None, Type::Int),            // start (required)
                (None, Type::seq(Type::Int)), // variadic rest (optional end)
            ],
            ret: Box::new(Type::seq(Type::Int)),
            variadic: true,
        },
    );
    // builtin-repeat: ∀T. T → Seq(T)
    env.insert_scheme(
        "builtin-repeat".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![(None, Type::TypeVar("T".to_string(), 0))],
                ret: Box::new(Type::seq(Type::TypeVar("T".to_string(), 0))),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-cycle: ∀T. Seq(T) → Seq(T)
    env.insert_scheme(
        "builtin-cycle".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![(None, Type::seq(Type::TypeVar("T".to_string(), 0)))],
                ret: Box::new(Type::seq(Type::TypeVar("T".to_string(), 0))),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-iterate: ∀T. (T → T) → T → Seq(T)
    env.insert_scheme(
        "builtin-iterate".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (
                        None,
                        Type::Function {
                            params: vec![(None, Type::TypeVar("T".to_string(), 0))],
                            ret: Box::new(Type::TypeVar("T".to_string(), 0)),
                            variadic: false,
                        },
                    ),
                    (None, Type::TypeVar("T".to_string(), 0)),
                ],
                ret: Box::new(Type::seq(Type::TypeVar("T".to_string(), 0))),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    env.insert(
        "builtin-unfold".to_string(),
        Type::Function {
            params: vec![
                (
                    None,
                    Type::Function {
                        params: vec![(None, Type::Top)],
                        ret: Box::new(Type::Top),
                        variadic: false,
                    },
                ),
                (None, Type::Top),
            ],
            ret: Box::new(Type::seq(Type::Top)),
            variadic: false,
        },
    );

    // ── Sequences: transforms ─────────────────────────────────────────────────
    // map: ∀(f: Operator) a b. Mappable f ⇒ (a → b) → f a → f b
    // HKT type scheme with Operator-kinded variable f.
    // The UNIFY-APP-OPERATOR-SEQ rule (type_unify.rs) enables unification of
    // App(Operator(f), a) with concrete collection types like Seq(T).
    env.insert_scheme(
        "map".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string(), "b".to_string()],
            kind_vars: vec![("f".to_string(), Kind::Operator)],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (
                        Some("fn".to_string()),
                        Type::Function {
                            params: vec![(None, Type::TypeVar("a".to_string(), 0))],
                            ret: Box::new(Type::TypeVar("b".to_string(), 0)),
                            variadic: false,
                        },
                    ),
                    (
                        Some("xs".to_string()),
                        Type::App(
                            Box::new(Type::Operator("f".to_string())),
                            Box::new(Type::TypeVar("a".to_string(), 0)),
                        ),
                    ),
                ],
                ret: Box::new(Type::App(
                    Box::new(Type::Operator("f".to_string())),
                    Box::new(Type::TypeVar("b".to_string(), 0)),
                )),
                variadic: false,
            },
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        },
    );
    // filter: ∀a. (a → Bool) → Seq a → Seq a
    env.insert_scheme(
        "filter".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (
                        Some("pred".to_string()),
                        Type::Function {
                            params: vec![(None, Type::TypeVar("a".to_string(), 0))],
                            ret: Box::new(Type::Bool),
                            variadic: false,
                        },
                    ),
                    (
                        Some("xs".to_string()),
                        Type::seq(Type::TypeVar("a".to_string(), 0)),
                    ),
                ],
                ret: Box::new(Type::seq(Type::TypeVar("a".to_string(), 0))),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // take: ∀T. Int → Seq(T) → Seq(T)
    env.insert_scheme(
        "take".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (None, Type::Int),
                    (None, Type::seq(Type::TypeVar("T".to_string(), 0))),
                ],
                ret: Box::new(Type::seq(Type::TypeVar("T".to_string(), 0))),
                variadic: false,
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
                    (None, Type::seq(Type::TypeVar("T".to_string(), 0))),
                ],
                ret: Box::new(Type::seq(Type::TypeVar("T".to_string(), 0))),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // drop: ∀T. Int → Seq(T) → Seq(T)
    env.insert_scheme(
        "drop".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (None, Type::Int),
                    (None, Type::seq(Type::TypeVar("T".to_string(), 0))),
                ],
                ret: Box::new(Type::seq(Type::TypeVar("T".to_string(), 0))),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // ── Sequences: reductions ─────────────────────────────────────────────────
    // reduce: ∀a b. (b → a → b) → b → Seq a → b
    env.insert_scheme(
        "reduce".to_string(),
        TypeScheme {
            type_vars: vec!["a".to_string(), "b".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (
                        Some("fn_".to_string()),
                        Type::Function {
                            params: vec![
                                (None, Type::TypeVar("b".to_string(), 0)),
                                (None, Type::TypeVar("a".to_string(), 0)),
                            ],
                            ret: Box::new(Type::TypeVar("b".to_string(), 0)),
                            variadic: false,
                        },
                    ),
                    (Some("init".to_string()), Type::TypeVar("b".to_string(), 0)),
                    (
                        Some("xs".to_string()),
                        Type::seq(Type::TypeVar("a".to_string(), 0)),
                    ),
                ],
                ret: Box::new(Type::TypeVar("b".to_string(), 0)),
                variadic: false,
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
                (None, Type::seq(Type::Top)),
            ],
            ret: Box::new(Type::Str),
            variadic: false,
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
                vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
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
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // ── Sequences: list operations ────────────────────────────────────────────
    // builtin-rest: ∀T. (Seq(T) | Record({})) → (Seq(T) | Record({}))
    // The implementation delegates to builtin-tail for Seq inputs and reindexes Dict inputs.
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
                        Type::seq(Type::TypeVar("T".to_string(), 0)),
                        Type::Record(Row {
                            fields: BTreeMap::new(),
                            tail: crate::type_def::RowTail::Empty,
                        }),
                    ]),
                )],
                ret: Box::new(Type::Union(vec![
                    Type::seq(Type::TypeVar("T".to_string(), 0)),
                    Type::Record(Row {
                        fields: BTreeMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ])),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-cons: ∀T. T → Seq[T] → Seq[T]
    env.insert_scheme(
        "builtin-cons".to_string(),
        TypeScheme {
            type_vars: vec!["T".to_string()],
            constraints: vec![],
            body: Type::Function {
                params: vec![
                    (None, Type::TypeVar("T".to_string(), 0)),
                    (None, Type::seq(Type::TypeVar("T".to_string(), 0))),
                ],
                ret: Box::new(Type::seq(Type::TypeVar("T".to_string(), 0))),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // builtin-reverse: Dict -> Dict
    env.insert(
        "builtin-reverse".to_string(),
        Type::Function {
            params: vec![(
                None,
                Type::Record(Row {
                    fields: BTreeMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }),
            )],
            ret: Box::new(Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            })),
            variadic: false,
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
                params: vec![(None, Type::seq(Type::TypeVar("T".to_string(), 0)))],
                ret: Box::new(Type::seq(Type::TypeVar("T".to_string(), 0))),
                variadic: false,
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
                    ret: Box::new(Type::Top),
                    variadic: false,
                },
            )],
            ret: Box::new(Type::Proxy),
            variadic: false,
        },
    );

    // ── builtin-get / get: Indexable c k v => k -> c -> v ────────────────────
    for get_name in ["builtin-get", "get"] {
        env.insert_scheme(
            get_name.to_string(),
            TypeScheme {
                type_vars: vec!["c".to_string(), "k".to_string(), "v".to_string()],
                constraints: vec![Constraint::Class {
                    class: Arc::clone(&indexable_class),
                    vars: vec!["c".to_string(), "k".to_string(), "v".to_string()],
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
                },
                label_vars: vec![],
                kind_vars: Vec::new(),
                doc: None,
                inner_schemes: None,
            },
        );
    }

    // get?: Indexable c k v => k -> c -> v | Null
    env.insert_scheme(
        "get?".to_string(),
        TypeScheme {
            type_vars: vec!["c".to_string(), "k".to_string(), "v".to_string()],
            constraints: vec![Constraint::Class {
                class: Arc::clone(&indexable_class),
                vars: vec!["c".to_string(), "k".to_string(), "v".to_string()],
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
                        fields: BTreeMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                ])),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );

    // ── Iteration builtins ────────────────────────────────────────────────────
    // each: Record → Seq Top
    env.insert_scheme(
        "each".to_string(),
        TypeScheme {
            type_vars: vec![],
            constraints: vec![],
            body: Type::Function {
                params: vec![(
                    Some("xs".to_string()),
                    Type::Record(Row {
                        fields: BTreeMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                )],
                ret: Box::new(Type::seq(Type::Top)),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // each-key: Record → Seq (Int | Str)
    env.insert_scheme(
        "each-key".to_string(),
        TypeScheme {
            type_vars: vec![],
            constraints: vec![],
            body: Type::Function {
                params: vec![(
                    Some("xs".to_string()),
                    Type::Record(Row {
                        fields: BTreeMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                )],
                ret: Box::new(Type::seq(Type::normalize_union(vec![Type::Int, Type::Str]))),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
        },
    );
    // each-kv: Record → Seq [key: Int | Str, value: Top]
    env.insert_scheme(
        "each-kv".to_string(),
        TypeScheme {
            type_vars: vec![],
            constraints: vec![],
            body: Type::Function {
                params: vec![(
                    Some("xs".to_string()),
                    Type::Record(Row {
                        fields: BTreeMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }),
                )],
                ret: Box::new(Type::seq({
                    let mut kv_fields = BTreeMap::new();
                    kv_fields.insert(
                        "key".to_string(),
                        Type::normalize_union(vec![Type::Int, Type::Str]),
                    );
                    kv_fields.insert("value".to_string(), Type::Top);
                    Type::Record(Row {
                        fields: kv_fields,
                        tail: crate::type_def::RowTail::Empty,
                    })
                })),
                variadic: false,
            },
            label_vars: vec![],
            kind_vars: Vec::new(),
            doc: None,
            inner_schemes: None,
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
                params: vec![(None, Type::seq(Type::TypeVar("T".to_string(), 0)))],
                ret: Box::new(Type::TypeVar("T".to_string(), 0)),
                variadic: false,
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
                params: vec![(None, Type::seq(Type::TypeVar("T".to_string(), 0)))],
                ret: Box::new(Type::TypeVar("T".to_string(), 0)),
                variadic: false,
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
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top), // Task — opaque async handle
            variadic: false,
        },
    );
    // builtin-await: Top → Top
    // Awaits a Task and returns its result.
    env.insert(
        "builtin-await".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top), // Task result — genuinely opaque
            variadic: false,
        },
    );
    // builtin-channel: Int → Top
    // Creates a buffered channel with the given capacity.
    env.insert(
        "builtin-channel".to_string(),
        Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Top), // Channel — opaque
            variadic: false,
        },
    );
    // builtin-send: Top → Top → Top
    // Sends a value on a channel: (channel, value) → Null/result
    env.insert(
        "builtin-send".to_string(),
        Type::Function {
            params: vec![(None, Type::Top), (None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    // builtin-recv: Top → Top
    // Receives a value from a channel.
    env.insert(
        "builtin-recv".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top), // Received value — genuinely opaque
            variadic: false,
        },
    );
    // builtin-select-once: Top → Top → Top
    // Selects the first ready channel from a seq of SelectSource values.
    // First arg is a context (for cancellation), second arg is the sources Seq.
    env.insert(
        "builtin-select-once".to_string(),
        Type::Function {
            params: vec![(None, Type::Top), (None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    // builtin-broadcast-channel: Int → Top
    // Creates a broadcast channel of the given capacity.
    // Each subscriber (via recv) receives every value sent after it subscribes.
    env.insert(
        "builtin-broadcast-channel".to_string(),
        Type::Function {
            params: vec![(None, Type::Int)],
            ret: Box::new(Type::Top), // BroadcastChannel — opaque
            variadic: false,
        },
    );
    // builtin-oneshot-channel: () → Top
    // Creates a one-shot channel (single send/recv pair).
    // Returns a dict with sender and receiver fields.
    env.insert(
        "builtin-oneshot-channel".to_string(),
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Top), // {sender, receiver} dict — opaque
            variadic: false,
        },
    );
    // builtin-try-send: Top → Top → Top
    // Non-blocking send: returns [Ok null] if sent, [Full] if buffer is full.
    env.insert(
        "builtin-try-send".to_string(),
        Type::Function {
            params: vec![(None, Type::Top), (None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    // builtin-par: Top → Top → Top
    // Runs two thunks in parallel, returns both results.
    env.insert(
        "builtin-par".to_string(),
        Type::Function {
            params: vec![(None, Type::Top), (None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
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
                        },
                    ),
                    (None, Type::seq(Type::TypeVar("a".to_string(), 0))),
                ],
                ret: Box::new(Type::seq(Type::TypeVar("b".to_string(), 0))),
                variadic: false,
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
                            ret: Box::new(Type::Bool),
                            variadic: false,
                        },
                    ),
                    (None, Type::seq(Type::TypeVar("a".to_string(), 0))),
                ],
                ret: Box::new(Type::seq(Type::TypeVar("a".to_string(), 0))),
                variadic: false,
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
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top), // Channel — opaque
            variadic: false,
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
            ret: Box::new(Type::Top), // Channel — opaque, no Channel type variant
            variadic: false,
        },
    );
    // builtin-watch-channel: DirCap → String → Channel (opaque)
    // Filesystem watch channel — sends null when the file/directory at path changes.
    // Returns Channel@Null — opaque (no Channel type variant in the type system).
    env.insert(
        "builtin-watch-channel".to_string(),
        Type::Function {
            params: vec![(None, Type::DirCap), (None, Type::Str)],
            ret: Box::new(Type::Top), // Channel@Null — opaque, no Channel type variant
            variadic: false,
        },
    );
    // builtin-context: Top → Top
    // Returns the current task context. Typically called as (builtin-context).
    // Typed as 1-arg to satisfy Function requirement; arg is variadic-style optional.
    env.insert(
        "builtin-context".to_string(),
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Top), // Context — opaque
            variadic: true,
        },
    );
    // builtin-with-cancel: Top → Top
    // Creates a cancellable child context from a parent context.
    env.insert(
        "builtin-with-cancel".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top), // (Context, cancel-fn) pair — opaque
            variadic: false,
        },
    );
    // builtin-with-timeout: ClockCap → Top → Duration → Top
    // Creates a context with a timeout; (ClockCap, parent-context, duration) → Context
    env.insert(
        "builtin-with-timeout".to_string(),
        Type::Function {
            params: vec![
                (None, Type::ClockCap),
                (None, Type::Top),
                (None, Type::Duration),
            ],
            ret: Box::new(Type::Top), // Context — opaque
            variadic: false,
        },
    );
    // builtin-with-deadline: ClockCap → Top → Timestamp → Top
    // Creates a context with a deadline; (ClockCap, parent-context, deadline) → Context
    env.insert(
        "builtin-with-deadline".to_string(),
        Type::Function {
            params: vec![
                (None, Type::ClockCap),
                (None, Type::Top),
                (None, Type::Timestamp),
            ],
            ret: Box::new(Type::Top), // Context — opaque
            variadic: false,
        },
    );
    // builtin-cancelled?: Top → Bool
    // Checks whether a context has been cancelled.
    env.insert(
        "builtin-cancelled-q".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Bool),
            variadic: false,
        },
    );
    // builtin-cancel-task: Top → Top
    // Cancels a task or context.
    env.insert(
        "builtin-cancel-task".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    // builtin-non-cancellable: Top → Top
    // Wraps a thunk so it runs in a non-cancellable context.
    env.insert(
        "builtin-non-cancellable".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    // builtin-with-context: Top → Top → Top
    // Runs a thunk with a specific context: (context, thunk) → result
    env.insert(
        "builtin-with-context".to_string(),
        Type::Function {
            params: vec![(None, Type::Top), (None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );
    // builtin-cancel-root: Top → Top
    // Cancels the root context of the current task.
    env.insert(
        "builtin-cancel-root".to_string(),
        Type::Function {
            params: vec![],
            ret: Box::new(Type::Top),
            variadic: true,
        },
    );
    // builtin-drain: Top → Top
    // Drains a channel, consuming all buffered values.
    env.insert(
        "builtin-drain".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
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
            params: vec![(None, Type::Top)], // any expression — not materialized
            ret: Box::new(Type::Top),        // metadata Dict — shape runtime-dependent
            variadic: false,
        },
    );

    // ── Type constructors ─────────────────────────────────────────────────────
    // Map with Unknown K/V is the unparameterized Map type.
    env.insert("Map".to_string(), Type::map(Type::Unknown, Type::Unknown));
    // Map[K V] as a parameterized type alias.
    env.insert_type_alias(
        "Map".to_string(),
        TypeAlias::new(
            vec!["k".to_string(), "v".to_string()],
            Type::map(
                Type::TypeVar("k".to_string(), 0),
                Type::TypeVar("v".to_string(), 0),
            ),
        ),
    );

    // ── Capability and handle type aliases ────────────────────────────────────
    // Register as type aliases so @DirCap, @NetCap, @Handle are valid in user annotations.
    env.insert_type_alias("DirCap".to_string(), TypeAlias::new(vec![], Type::DirCap));
    env.insert_type_alias("NetCap".to_string(), TypeAlias::new(vec![], Type::NetCap));
    env.insert_type_alias(
        "Handle".to_string(),
        TypeAlias::new(vec![], Type::handle(Type::Unknown)),
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
        let mut fields = BTreeMap::new();
        fields.insert(
            format!("__cap_flag_{}", flag_name.to_lowercase()),
            Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            }),
        );
        env.insert_type_alias(
            flag_name.to_string(),
            TypeAlias::new(
                vec![],
                Type::Record(Row {
                    fields,
                    tail: crate::type_def::RowTail::Empty,
                }),
            ),
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
        let mut fields = BTreeMap::new();
        fields.insert(
            format!("__cap_flag_{}", flag_name.to_lowercase()),
            Type::Record(Row {
                fields: BTreeMap::new(),
                tail: crate::type_def::RowTail::Empty,
            }),
        );
        env.insert_type_alias(
            flag_name.to_string(),
            TypeAlias::new(
                vec![],
                Type::Record(Row {
                    fields,
                    tail: crate::type_def::RowTail::Empty,
                }),
            ),
        );
    }

    // ── HashAlgorithm type alias ──────────────────────────────────────────────
    // Union of string literal types for hash algorithm identifiers.
    // Used as the algorithm argument to hash and SPKI pin functions.
    env.insert_type_alias(
        "HashAlgorithm".to_string(),
        TypeAlias::new(
            vec![],
            Type::normalize_union(vec![
                Type::StringLiteral("Sha256".to_string()),
                Type::StringLiteral("Sha384".to_string()),
                Type::StringLiteral("Sha512".to_string()),
                Type::StringLiteral("Sha3-256".to_string()),
                Type::StringLiteral("Sha3-384".to_string()),
                Type::StringLiteral("Sha3-512".to_string()),
                Type::StringLiteral("Blake3".to_string()),
            ]),
        ),
    );

    // ── builtin-* aliases for core operators ─────────────────────────────────
    // When a document declares `--- uses: ["core"]`, `type_env_module("core")`
    // injects this env. User code may then use `builtin-if`, `builtin-add`, etc.
    // directly (bypassing prelude). These aliases must mirror the canonical forms
    // already registered above so that the type checker accepts them.
    //
    // Note: aliases that already have their own direct registrations above
    // (e.g. `builtin-get`, `builtin-take`, `builtin-keys`, `builtin-each`, etc.)
    // are NOT included here — the loop would overwrite them with the canonical
    // form's TypeScheme instead of their own more-precise scheme.
    for (alias, canonical) in [
        // Arithmetic operators
        ("builtin-add", "+"),
        ("builtin-sub", "-"),
        ("builtin-mul", "*"),
        ("builtin-div", "/"),
        // Comparison operators
        ("builtin-eq", "="),
        ("builtin-lt", "<"),
        ("builtin-gt", ">"),
        ("builtin-gte", ">="),
        ("builtin-lte", "<="),
        // Control flow
        ("builtin-if", "if"),
        // Sequence transforms
        ("builtin-filter", "filter"),
        ("builtin-map", "map"),
        ("builtin-reduce", "reduce"),
        ("builtin-drop", "drop"),
        // String operations
        ("builtin-str", "str"),
        ("builtin-split", "split"),
        ("builtin-trim", "trim"),
        ("builtin-trim-start", "trim-start"),
        ("builtin-trim-end", "trim-end"),
        ("builtin-replace", "replace"),
        ("builtin-str-chars", "str-chars"),
        ("builtin-char-code", "char-code"),
        ("builtin-chr", "chr"),
        ("builtin-str-bytes", "str-bytes"),
        ("builtin-bytes-str", "bytes-str"),
        ("builtin-str-index-of", "str-index-of"),
        ("builtin-str-to-upper-char", "str-to-upper-char"),
        ("builtin-str-to-lower-char", "str-to-lower-char"),
        ("builtin-str-map-chars", "str-map-chars"),
        ("builtin-str-length", "str-length"),
        ("builtin-str-slice", "str-slice"),
        ("builtin-regex-match?", "regex-match?"),
        // Numeric
        ("builtin-floor", "floor"),
        ("builtin-round", "round"),
        ("builtin-to-int", "to-int"),
        ("builtin-to-float", "to-float"),
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
        ("builtin-band", "band"),
        ("builtin-bor", "bor"),
        ("builtin-bxor", "bxor"),
        ("builtin-shl", "shl"),
        ("builtin-shr", "shr"),
        ("builtin-float", "float"),
        // Control/meta (non-direct registrations)
        ("builtin-apply", "apply"),
        ("builtin-try", "try"),
        ("builtin-type-of", "type-of"),
        ("builtin-raise", "raise"),
        ("builtin-to-tinct", "to-tinct"),
        ("builtin-llt-repr", "llt-repr"),
        ("builtin-tag-of", "tag-of"),
        ("builtin-eval-ast", "eval-ast"),
        ("builtin-gensym", "gensym"),
        ("builtin-decimal", "decimal"),
        ("builtin-big-int", "big-int"),
        ("builtin-variant", "variant"),
        ("builtin-annotation-of", "annotation-of"),
        ("builtin-make-annotated", "make-annotated"),
        // I/O (bare-name forms)
        ("builtin-emit", "emit"),
        ("builtin-env", "env"),
        ("builtin-narrow", "narrow"),
        ("builtin-write", "write"),
        ("builtin-write-atomic", "write-atomic"),
        ("builtin-flush", "flush"),
        ("builtin-close", "close"),
        ("builtin-stat", "stat"),
        ("builtin-make-dir", "make-dir"),
        ("builtin-rename", "rename"),
        ("builtin-link", "link"),
        ("builtin-read-link", "read-link"),
        ("builtin-seek", "seek"),
        ("builtin-list-dir", "list-dir"),
        ("builtin-open", "open"),
    ] {
        if let Some(scheme) = env.get(canonical).cloned() {
            env.insert_scheme(alias.to_string(), scheme);
        }
    }
}
