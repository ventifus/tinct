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
// Arithmetic and comparison implementations — Core-46 only.
use crate::builtins_math::{
    builtin_eq_int,
    builtin_eq_string,
    builtin_float_add,
    builtin_float_gt,
    builtin_float_gte,
    builtin_float_mul,
    builtin_float_sub,
    builtin_float_to_int,
    // Monomorphic typed variants.
    builtin_int_add,
    builtin_int_gt,
    builtin_int_gte,
    builtin_int_mul,
    builtin_int_sub,
    builtin_int_to_float,
    builtin_lt,
    builtin_str_gt,
    builtin_str_gte,
};
// Dict/access implementations — all stay in core.
use crate::builtins_dict::{
    builtin_build_dict, builtin_builder_delete, builtin_builder_finish, builtin_builder_get,
    builtin_builder_get_or, builtin_builder_has, builtin_builder_set, builtin_builder_snapshot,
    builtin_dict_has_key_nth, builtin_dict_has_kv_nth, builtin_dict_has_nth, builtin_dict_key_nth,
    builtin_dict_kv_nth, builtin_dict_merge, builtin_dict_nth, builtin_get, builtin_get_by_field,
    builtin_has_key, builtin_keys, builtin_length, builtin_make_builder,
};
// String implementations — Core-46 only.
use crate::builtins_string::{
    builtin_bytes_str, builtin_int_to_string, builtin_str_bytes, builtin_str_index_of,
    builtin_str_length, builtin_str_slice, builtin_string_concat,
};
// Bytes implementations — Core-46 only.
use crate::builtins_bytes::{builtin_bytes, builtin_bytes_concat};
// Meta/eval implementations — Core-46 only.
use crate::builtins_meta::{
    builtin_builtin_module, builtin_cap_env_has, builtin_check_type, builtin_debug_repr,
    builtin_desugar, builtin_doc_expressions, builtin_doc_meta, builtin_eval,
    builtin_get_type_context, builtin_is_variant, builtin_llt_repr, builtin_parse,
    builtin_program_docs, builtin_raise, builtin_resolve, builtin_tag_of, builtin_try,
    builtin_type_of, builtin_typecheck_doc, builtin_variant_payload,
};
// I/O implementations — Core-46 only.
use crate::builtins_dict::{builtin_concat, builtin_drop, builtin_take};
use crate::builtins_io::{
    builtin_file_open, builtin_file_read, builtin_list_dir, builtin_narrow, builtin_write_stderr,
    builtin_write_stdout,
};
// Async concurrency implementations — Core-46 only (channel, send).
use crate::builtins_async::{builtin_channel, builtin_send};

use crate::value::{BuiltinDef, Strictness};

/// Returns all "core" module Rust builtins aggregated from the split implementation files.
pub fn core_builtins() -> Vec<BuiltinDef> {
    vec![
        // ── Arithmetic — monomorphic typed variants ───────────────────────────────────
        builtin!(
            "builtin-int-add",
            builtin_int_add,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-float-add",
            builtin_float_add,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-int-to-float",
            builtin_int_to_float,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        builtin!(
            "builtin-float-to-int",
            builtin_float_to_int,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        builtin!(
            "builtin-int-sub",
            builtin_int_sub,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-float-sub",
            builtin_float_sub,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-int-mul",
            builtin_int_mul,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-float-mul",
            builtin_float_mul,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        // ── Comparison — monomorphic typed variants ───────────────────────────────────
        builtin!(
            "builtin-int-gt",
            builtin_int_gt,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-float-gt",
            builtin_float_gt,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-str-gt",
            builtin_str_gt,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-int-gte",
            builtin_int_gte,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-float-gte",
            builtin_float_gte,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-str-gte",
            builtin_str_gte,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        // ── Comparison ───────────────────────────────────────────────────────────────
        builtin!(
            "builtin-eq-int",
            builtin_eq_int,
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
        // ── Dict primitives ──────────────────────────────────────────────────────────
        builtin!("builtin-keys", builtin_keys, [Strictness::Spine], 1, ["xs"]),
        builtin!(
            "builtin-dict-length",
            builtin_length,
            [Strictness::Spine],
            1,
            ["xs"]
        ),
        builtin!(
            "builtin-bytes-length",
            builtin_length,
            [Strictness::Spine],
            1,
            ["xs"]
        ),
        builtin!(
            "builtin-dict-get",
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
        builtin!(
            "builtin-dict-merge",
            builtin_dict_merge,
            [Strictness::Spine, Strictness::Spine],
            2,
            ["a", "b"]
        ),
        // ── Builder ops ──────────────────────────────────────────────────────────────
        builtin!("builtin-make-builder", builtin_make_builder),
        builtin!(
            "builtin-builder-set",
            builtin_builder_set,
            [Strictness::Seq, Strictness::Id, Strictness::Seq],
            0,
            ["key", "value", "builder"]
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
        // ── String ops (Core-46 only) ────────────────────────────────────────────────
        builtin!(
            "builtin-int->string",
            builtin_int_to_string,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        builtin!(
            "builtin-str-length",
            builtin_str_length,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-str-slice",
            builtin_str_slice,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["str", "start", "end"]
        ),
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
            "builtin-string-concat",
            builtin_string_concat,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        // ── Bytes (Core-46 only) ─────────────────────────────────────────────────────
        builtin!("builtin-bytes", builtin_bytes, []),
        builtin!(
            "builtin-bytes-concat",
            builtin_bytes_concat,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        // ── Control flow ─────────────────────────────────────────────────────────────
        // The builtin! macro requires a literal; BUILTIN_RAISE_NAME is verified against
        // this literal by test_builtin_raise_name_registered_in_core_builtins in lower.rs.
        builtin!(
            "builtin-raise",
            builtin_raise,
            [Strictness::Seq],
            0,
            ["diag"]
        ),
        // builtin-try: takes 1 zero-arg function, calls it, returns `{ok: value}` on success
        // or the unified diagnostic dict `{level, kind, message, span, ...}` on failure.
        // See builtin_try in builtins_meta.rs.
        builtin!("builtin-try", builtin_try, [], 0, ["expr"]),
        // ── Type introspection ────────────────────────────────────────────────────────
        builtin!(
            "builtin-type-of",
            builtin_type_of,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!(
            "builtin-check-type",
            builtin_check_type,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["type-name", "x"]
        ),
        // ── Caps/environment ─────────────────────────────────────────────────────────
        builtin!(
            "builtin-cap-env-has?",
            builtin_cap_env_has,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["name", "env"]
        ),
        // ── I/O (Core-46 only) ────────────────────────────────────────────────────────
        builtin!(
            "builtin-narrow",
            builtin_narrow,
            [Strictness::Seq, Strictness::Seq],
            0,
            ["cap", "path"]
        ),
        builtin!(
            "builtin-list-dir",
            builtin_list_dir,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["cap", "path"]
        ),
        builtin!(
            "builtin-file-open",
            builtin_file_open,
            [
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq
            ],
            5,
            ["cap", "path", "modes", "mode", "flags"]
        ),
        builtin!(
            "builtin-file-read",
            builtin_file_read,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["file", "n"]
        ),
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
        // ── Pipeline primitives ───────────────────────────────────────────────────────
        builtin!(
            "builtin-parse",
            builtin_parse,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["bytes", "path"]
        ),
        builtin!(
            "builtin-resolve",
            builtin_resolve,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["doc", "frames"]
        ),
        // Pipeline stage lint primitive
        builtin!(
            "builtin-lint-pipeline-docs",
            crate::builtins_meta::builtin_lint_pipeline_docs,
            [Strictness::Seq],
            1,
            ["docs"]
        ),
        builtin!(
            "builtin-typecheck-doc",
            builtin_typecheck_doc,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["doc", "tc", "doc-env"]
        ),
        // TypeContext primitives
        builtin!("builtin-get-type-context", builtin_get_type_context, [], 0),
        builtin!(
            "builtin-make-type-ctx",
            crate::builtins_meta::builtin_make_type_ctx,
            [],
            0
        ),
        builtin!(
            "builtin-tc-update-type-stage-env",
            crate::builtins_meta::builtin_tc_update_type_stage_env,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["tc", "env-dict"]
        ),
        builtin!(
            "builtin-module",
            builtin_builtin_module,
            [Strictness::Seq],
            1,
            ["name"]
        ),
        builtin!(
            "builtin-eval",
            builtin_eval,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["doc", "env-dict"]
        ),
        builtin!(
            "builtin-variant-payload",
            builtin_variant_payload,
            [Strictness::Seq],
            1,
            ["variant"]
        ),
        // ── Program/document decomposition ────────────────────────────────────────────
        builtin!(
            "builtin-desugar",
            builtin_desugar,
            [Strictness::Seq],
            0,
            ["program"]
        ),
        builtin!(
            "builtin-program-docs",
            builtin_program_docs,
            [Strictness::Seq],
            0,
            ["program"]
        ),
        builtin!(
            "builtin-doc-meta",
            builtin_doc_meta,
            [Strictness::Seq, Strictness::Seq],
            0,
            ["doc", "env"]
        ),
        builtin!(
            "builtin-doc-expressions",
            builtin_doc_expressions,
            [Strictness::Seq],
            0,
            ["doc"]
        ),
        // ── Sequences ─────────────────────────────────────────────────────────────────
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
        // ── Async concurrency (Core-46 only) ─────────────────────────────────────────
        builtin!("builtin-channel", builtin_channel),
        builtin!("builtin-send", builtin_send),
        // ── Meta / reflection (Core-46 only) ─────────────────────────────────────────
        builtin!(
            "builtin-llt-repr",
            builtin_llt_repr,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!(
            "builtin-debug-repr",
            builtin_debug_repr,
            [Strictness::Seq],
            0,
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
            "builtin-variant?",
            builtin_is_variant,
            [Strictness::Seq],
            0,
            ["x"]
        ),
    ]
}
