//! Unified environment: merged `TypeEnv` (type checker) and `Environment` (evaluator)
//! into a single `Env` struct used by both.
//!
//! After T-1557, `Env` is **type-metadata only**. Runtime values are stored exclusively
//! in `FlatEnv` (arena-complete). Each slot holds an optional type scheme (populated at
//! typecheck time). De Bruijn (level, slot) indices address the same `IndexMap` position
//! for both subsystems — no coordinate-system mismatch.
//!
//! Parent chain uses `Arc<RwLock<Env>>` (no `Rc` anywhere).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::type_class::{ClassDecl, InstanceDecl};
use crate::types::{Type, TypeAlias, TypeScheme};

// ---------------------------------------------------------------------------
// EnvSlot
// ---------------------------------------------------------------------------

/// A single binding slot: scheme side (type checker) only.
///
/// After T-1557, runtime values are stored exclusively in `FlatEnv` (arena-complete).
/// `Env` is type-metadata only.
#[derive(Debug, Clone)]
pub struct EnvSlot {
    pub scheme: Option<TypeScheme>,
}

// ---------------------------------------------------------------------------
// Env
// ---------------------------------------------------------------------------

/// Unified scope frame used by the type checker (type-metadata only after T-1557).
///
/// `slots` is an `IndexMap` so that position N aligns with the type checker
/// (`EnvSlot.scheme`). The resolver assigns de Bruijn (level, slot) coordinates
/// that index directly into this map.
///
/// `extras` holds name-only entries with no resolver-assigned slot (builtins,
/// injected names). These are reachable via `get_scheme` but never via
/// `get_scheme_at`.
#[derive(Clone)]
pub struct Env {
    /// Slot-indexed entries: position N is the same for the type checker
    /// (EnvSlot.scheme). Resolver de Bruijn (level, slot) indexes this.
    pub(crate) slots: IndexMap<String, EnvSlot>,
    /// Name-only entries with no resolver-assigned slot (builtins, injected names).
    pub(crate) extras: HashMap<String, EnvSlot>,
    /// Class declarations registered in this scope frame.
    pub(crate) classes: IndexMap<String, ClassDecl>,
    /// Instance declarations keyed by mangled name (e.g., "ɪɴꜱᴛᴀɴᴄᴇ⧼Equatable∷=⟨Int⟩⧽").
    pub(crate) instances: IndexMap<String, InstanceDecl>,
    /// Type alias declarations.
    pub(crate) type_aliases: HashMap<String, TypeAlias>,
    /// Parent scope frame.
    pub parent: Option<Arc<RwLock<Env>>>,
}

impl Env {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    /// Create an empty environment with no parent.
    pub fn new() -> Self {
        Self {
            slots: IndexMap::new(),
            extras: HashMap::new(),
            classes: IndexMap::new(),
            instances: IndexMap::new(),
            type_aliases: HashMap::new(),
            parent: None,
        }
    }

    /// Create an empty environment with the given parent.
    pub fn with_parent(parent: Arc<RwLock<Env>>) -> Self {
        Self {
            slots: IndexMap::new(),
            extras: HashMap::new(),
            classes: IndexMap::new(),
            instances: IndexMap::new(),
            type_aliases: HashMap::new(),
            parent: Some(parent),
        }
    }

    /// Return the keys of the slots IndexMap as a `Vec<String>`.
    ///
    /// Used by the resolver to seed scope frames.
    pub fn slot_names(&self) -> Vec<String> {
        self.slots.keys().cloned().collect()
    }

    /// Iterate over all slot entries in the current frame.
    pub fn iter_slots(&self) -> impl Iterator<Item = (&str, &EnvSlot)> {
        self.slots.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Check whether a name is present in this environment (slots or extras), walking the
    /// parent chain.
    ///
    /// Returns `true` if the name was registered with `insert_slot_name_only`,
    /// `insert_scheme`, or `insert_scheme_named_only` — regardless of whether a
    /// `TypeScheme` is associated. This is a name-presence test only; it does not force
    /// any runtime thunk.
    pub fn has_name(&self, name: &str) -> bool {
        if self.slots.contains_key(name) || self.extras.contains_key(name) {
            return true;
        }
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_arc) = current {
            let env = env_arc.read().unwrap();
            if env.slots.contains_key(name) || env.extras.contains_key(name) {
                return true;
            }
            current = env.parent.as_ref().map(Arc::clone);
        }
        false
    }

    /// Find the first slot key ending with `suffix`, walking the parent chain.
    ///
    /// Used by `resolve_matchable_binding_from_fn` to discover the Matchable class
    /// instance binding for a given type name without knowing the class name ahead of time.
    pub fn find_key_with_suffix(&self, suffix: &str) -> Option<String> {
        // Check current frame
        if let Some(name) = self.slots.keys().find(|n| n.ends_with(suffix)) {
            return Some(name.clone());
        }
        // Walk parent chain
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_arc) = current {
            let env = env_arc.read().unwrap();
            if let Some(name) = env.slots.keys().find(|n| n.ends_with(suffix)) {
                return Some(name.clone());
            }
            current = env.parent.as_ref().map(Arc::clone);
        }
        None
    }

    // -----------------------------------------------------------------------
    // SCHEME SIDE (from TypeEnv)
    // -----------------------------------------------------------------------

    /// Insert a name-only slot (no TypeScheme) into the slotted IndexMap.
    ///
    /// This is used by `build_core_env` to register builtin names so the resolver
    /// can assign de Bruijn (level, slot) coordinates without requiring a TypeScheme
    /// at bootstrap time. The runtime thunk goes in the root FlatEnv at the same
    /// slot position (see `EvalContext::new_env_arena`).
    ///
    /// If a slot already exists for `name`, this is a no-op (the existing entry,
    /// including any scheme, is preserved). If no slot exists, inserts
    /// `EnvSlot { scheme: None }`.
    pub fn insert_slot_name_only(&mut self, name: String) {
        if !self.slots.contains_key(&name) {
            self.slots.insert(name, EnvSlot { scheme: None });
        }
    }

    /// Insert a TypeScheme into the slotted IndexMap.
    ///
    /// If a slot already exists for `name`, sets its `.scheme`; otherwise creates
    /// a new slot. IndexMap preserves insertion order and updates in-place on
    /// duplicate keys, so the slot index of an existing entry is stable.
    pub fn insert_scheme(&mut self, name: String, scheme: TypeScheme) {
        if let Some(slot) = self.slots.get_mut(&name) {
            slot.scheme = Some(scheme);
        } else {
            self.slots.insert(name, EnvSlot { scheme: Some(scheme) });
        }
    }

    /// Insert a TypeScheme into the extras HashMap (name-only, no slot).
    ///
    /// Use this for entries NOT assigned a slot by the resolver:
    /// - ADT constructor type information
    /// - Class method injections
    /// - Narrowing overrides
    /// - Builtin bindings
    ///
    /// These entries are visible via `get_scheme` but never reached via `get_scheme_at`.
    pub fn insert_scheme_named_only(&mut self, name: String, scheme: TypeScheme) {
        if let Some(slot) = self.extras.get_mut(&name) {
            slot.scheme = Some(scheme);
        } else {
            self.extras.insert(name, EnvSlot { scheme: Some(scheme) });
        }
    }

    /// Look up a type scheme by name, walking slots then extras then the parent chain.
    ///
    /// Returns a cloned `TypeScheme` because the parent chain is behind
    /// `Arc<RwLock<Env>>` — references cannot outlive the lock guard.
    pub fn get_scheme(&self, name: &str) -> Option<TypeScheme> {
        // Check slots in current frame
        if let Some(slot) = self.slots.get(name) {
            if let Some(ref s) = slot.scheme {
                return Some(s.clone());
            }
        }
        // Check extras in current frame
        if let Some(slot) = self.extras.get(name) {
            if let Some(ref s) = slot.scheme {
                return Some(s.clone());
            }
        }
        // Walk parent chain iteratively
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_arc) = current {
            let env = env_arc.read().unwrap();
            if let Some(slot) = env.slots.get(name) {
                if let Some(ref s) = slot.scheme {
                    return Some(s.clone());
                }
            }
            if let Some(slot) = env.extras.get(name) {
                if let Some(ref s) = slot.scheme {
                    return Some(s.clone());
                }
            }
            current = env.parent.as_ref().map(Arc::clone);
        }
        None
    }

    /// Slot-indexed scheme lookup: walk `level` parent frames (0 = current),
    /// look up `slot` in the target frame's `slots` IndexMap, and return the
    /// TypeScheme.
    ///
    /// **NO name check, NO expected_name parameter.** This is intentional — the
    /// old `get_type_at(level, slot, expected_name)` name check was the bug causing
    /// class method type-warning false positives (slot key is the i-prefixed
    /// instance binding name, VarRef source name is the method name).
    ///
    /// Returns a cloned TypeScheme because the parent chain is behind
    /// `Arc<RwLock<Env>>`.
    pub fn get_scheme_at(&self, level: u32, slot: u32) -> Option<TypeScheme> {
        if level == 0 {
            return self
                .slots
                .get_index(slot as usize)
                .and_then(|(_, s)| s.scheme.clone());
        }
        let mut steps = level;
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_arc) = current {
            steps -= 1;
            if steps == 0 {
                let env = env_arc.read().unwrap();
                return env
                    .slots
                    .get_index(slot as usize)
                    .and_then(|(_, s)| s.scheme.clone());
            }
            let next = {
                let env = env_arc.read().unwrap();
                env.parent.as_ref().map(Arc::clone)
            };
            current = next;
        }
        None
    }

    /// Look up a binding in the CURRENT frame only (does not walk the parent chain).
    ///
    /// Returns a reference to the scheme since no lock traversal is needed.
    pub fn get_own_scheme(&self, name: &str) -> Option<&TypeScheme> {
        self.slots
            .get(name)
            .and_then(|s| s.scheme.as_ref())
            .or_else(|| self.extras.get(name).and_then(|s| s.scheme.as_ref()))
    }

    /// Convenience: wrap a `Type` in a monomorphic `TypeScheme` and insert into slots.
    pub fn insert(&mut self, name: String, ty: Type) {
        self.insert_scheme(name, TypeScheme::mono(ty));
    }

    // -----------------------------------------------------------------------
    // COMBINED
    // -----------------------------------------------------------------------

    /// Insert a scheme into a slot (formerly also stored the value; after T-1557, type-only).
    ///
    /// Callers that previously supplied a `thunk` argument should switch to `insert_scheme`.
    /// This signature is retained for callers that were already passing both; the thunk is
    /// no longer stored here — values go into `FlatEnv` (T-1559).
    #[allow(unused_variables)]
    pub fn insert_both(&mut self, name: String, thunk: std::sync::Arc<crate::value::Thunk>, scheme: TypeScheme) {
        self.insert_scheme(name, scheme);
    }

    // -----------------------------------------------------------------------
    // CLASS / INSTANCE
    // -----------------------------------------------------------------------

    /// Insert a class declaration into this frame, keyed by `decl.name`.
    /// If a class with the same name already exists in this frame, it is overwritten.
    pub fn insert_class(&mut self, decl: ClassDecl) {
        self.classes.insert(decl.name.clone(), decl);
    }

    /// Insert an instance declaration into this frame, keyed by `mangled_name`.
    /// The mangled name is the instance binding name used in the runtime env
    /// (e.g. `ɪɴꜱᴛᴀɴᴄᴇ⧼Addable Int Float Float⧽`). Idempotent: if an entry
    /// with this key already exists, it is overwritten.
    pub fn insert_instance(&mut self, mangled_name: String, decl: InstanceDecl) {
        self.instances.insert(mangled_name, decl);
    }

    /// Look up a class declaration by name, walking the parent chain (inner wins).
    /// Returns a clone so the caller is not restricted by borrow lifetimes.
    pub fn get_class(&self, name: &str) -> Option<ClassDecl> {
        if let Some(decl) = self.classes.get(name) {
            return Some(decl.clone());
        }
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_arc) = current {
            let env = env_arc.read().unwrap();
            if let Some(decl) = env.classes.get(name) {
                return Some(decl.clone());
            }
            current = env.parent.as_ref().map(Arc::clone);
        }
        None
    }

    /// Look up an instance declaration by mangled name, walking the parent chain (inner wins).
    pub fn get_instance(&self, mangled: &str) -> Option<InstanceDecl> {
        if let Some(decl) = self.instances.get(mangled) {
            return Some(decl.clone());
        }
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_arc) = current {
            let env = env_arc.read().unwrap();
            if let Some(decl) = env.instances.get(mangled) {
                return Some(decl.clone());
            }
            current = env.parent.as_ref().map(Arc::clone);
        }
        None
    }

    /// Collect all class declarations from the full parent chain, root-first.
    /// Later (inner) frames with the same class name overwrite earlier (outer) ones.
    /// Returns one `ClassDecl` per unique class name.
    pub fn all_classes(&self) -> Vec<ClassDecl> {
        let mut map: std::collections::BTreeMap<String, ClassDecl> =
            std::collections::BTreeMap::new();
        self.collect_all_classes_into(&mut map);
        map.into_values().collect()
    }

    fn collect_all_classes_into(&self, map: &mut std::collections::BTreeMap<String, ClassDecl>) {
        // Walk parent chain first (root-to-leaf), then overwrite with self (inner wins).
        if let Some(ref parent) = self.parent {
            let env = parent.read().unwrap();
            env.collect_all_classes_into(map);
        }
        for (name, decl) in &self.classes {
            map.insert(name.clone(), decl.clone());
        }
    }

    /// Collect all instance declarations from the full parent chain.
    /// Returns `(mangled_name, InstanceDecl)` pairs. Inner frames overwrite outer for
    /// the same mangled key.
    pub fn all_instances(&self) -> Vec<(String, InstanceDecl)> {
        let mut map: std::collections::BTreeMap<String, InstanceDecl> =
            std::collections::BTreeMap::new();
        self.collect_all_instances_into(&mut map);
        map.into_iter().collect()
    }

    fn collect_all_instances_into(
        &self,
        map: &mut std::collections::BTreeMap<String, InstanceDecl>,
    ) {
        if let Some(ref parent) = self.parent {
            let env = parent.read().unwrap();
            env.collect_all_instances_into(map);
        }
        for (mangled, decl) in &self.instances {
            map.insert(mangled.clone(), decl.clone());
        }
    }

    // -----------------------------------------------------------------------
    // TYPE ALIASES
    // -----------------------------------------------------------------------

    /// Insert a type alias declaration.
    pub fn insert_type_alias(&mut self, name: String, alias: TypeAlias) {
        self.type_aliases.insert(name, alias);
    }

    /// Look up a type alias by name, walking the parent chain.
    ///
    /// Returns a cloned `TypeAlias` because the parent chain is behind
    /// `Arc<RwLock<Env>>`.
    pub fn get_type_alias(&self, name: &str) -> Option<TypeAlias> {
        if let Some(alias) = self.type_aliases.get(name) {
            return Some(alias.clone());
        }
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_arc) = current {
            let env = env_arc.read().unwrap();
            if let Some(alias) = env.type_aliases.get(name) {
                return Some(alias.clone());
            }
            current = env.parent.as_ref().map(Arc::clone);
        }
        None
    }

    // -----------------------------------------------------------------------
    // UTILITIES
    // -----------------------------------------------------------------------

    /// Collect all binding names visible from this environment (including parent scopes).
    ///
    /// Walks the scope chain and inserts every bound name into `names`.
    pub fn collect_all_names(&self, names: &mut std::collections::HashSet<String>) {
        for name in self.slots.keys() {
            names.insert(name.clone());
        }
        for name in self.extras.keys() {
            names.insert(name.clone());
        }
        if let Some(ref parent) = self.parent {
            let env = parent.read().unwrap();
            env.collect_all_names(names);
        }
    }

    /// Collect only the binding names defined in THIS frame (no parent walk).
    /// Stub — TypeEnv-compatible method. Qualification of bare constructor tags
    /// is handled by the evaluator, not the type checker env.
    pub fn resolve_constructor_tag(&self, _tag: &str) -> Option<String> {
        None
    }

    pub fn collect_own_names(&self, names: &mut std::collections::HashSet<String>) {
        for name in self.slots.keys() {
            names.insert(name.clone());
        }
        for name in self.extras.keys() {
            names.insert(name.clone());
        }
    }

    /// Iterate over type aliases defined in THIS frame only (no parent walk).
    pub fn own_type_aliases(&self) -> impl Iterator<Item = (&str, &TypeAlias)> {
        self.type_aliases.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Register alias type schemes: for each `(alias, original)`, copy the scheme
    /// from `original` to `alias`. Aliases are inserted into extras (name-only, no slot).
    pub fn alias_types(&mut self, pairs: &[(&str, &str)]) {
        for &(alias, canonical) in pairs {
            if let Some(scheme) = self.get_scheme(canonical) {
                self.insert_scheme_named_only(alias.to_string(), scheme);
            }
        }
    }

    /// Copy all bindings and type aliases from `other` into `self`.
    ///
    /// Copies only the own (non-parent) bindings and type aliases from `other`.
    /// Parent chains are not traversed. Existing entries in `self` with the same
    /// name are overwritten by entries from `other`.
    pub fn merge(&mut self, other: Env) {
        for (name, slot) in other.slots {
            if let Some(existing) = self.slots.get_mut(&name) {
                // Merge: fill in whichever side the incoming slot provides.
                if slot.scheme.is_some() {
                    existing.scheme = slot.scheme;
                }
            } else {
                self.slots.insert(name, slot);
            }
        }
        for (name, slot) in other.extras {
            if let Some(existing) = self.extras.get_mut(&name) {
                if slot.scheme.is_some() {
                    existing.scheme = slot.scheme;
                }
            } else {
                self.extras.insert(name, slot);
            }
        }
        for (name, alias) in other.type_aliases {
            self.type_aliases.insert(name, alias);
        }
        for (name, decl) in other.classes {
            self.classes.insert(name, decl);
        }
        for (mangled, decl) in other.instances {
            self.instances.insert(mangled, decl);
        }
    }

    /// Inject `builtin-*` aliases into this type environment.
    ///
    /// These aliases map `builtin-lt` -> `<`, `builtin-add` -> `+`, etc.
    /// They are used by `stdlib/prelude.llt` to call Rust primitives by stable
    /// names that cannot be shadowed by user code.
    ///
    /// **Only call this when type-checking prelude itself.** User code does NOT
    /// have `builtin-*` names in scope.
    pub fn inject_builtin_aliases(&mut self) {
        for (alias, canonical) in [
            ("builtin-lt", "<"),
            ("builtin-gt", ">"),
            ("builtin-gte", ">="),
            ("builtin-lte", "<="),
            ("builtin-eq", "="),
            ("builtin-add", "+"),
            ("builtin-sub", "-"),
            ("builtin-mul", "*"),
            ("builtin-div", "/"),
            ("builtin-filter", "filter"),
            ("builtin-map", "map"),
            ("builtin-reduce", "reduce"),
            ("builtin-take", "take"),
            ("builtin-drop", "drop"),
            ("builtin-eval-ast", "eval-ast"),
            ("builtin-gensym", "gensym"),
            ("builtin-llt-repr", "llt-repr"),
            ("builtin-tag-of", "tag-of"),
            ("builtin-variant", "variant"),
            ("builtin-decimal", "decimal"),
            ("builtin-big-int", "big-int"),
            ("builtin-proxy", "proxy"),
            // prelude-missing-wrappers sprint: stable aliases for previously-unwrapped builtins
            ("builtin-keys", "keys"),
            ("builtin-merge", "merge"),
            ("builtin-each", "each"),
            ("builtin-each-key", "each-key"),
            ("builtin-each-kv", "each-kv"),
            ("builtin-floor", "floor"),
            ("builtin-round", "round"),
            ("builtin-to-float", "to-float"),
            ("builtin-try", "try"),
            ("builtin-apply", "apply"),
            ("builtin-type-of", "type-of"),
            ("builtin-narrow", "narrow"),
            // builtin-privacy-primary-names sprint: new builtin-* -> bare-name mappings
            ("builtin-raise", "raise"),
            ("builtin-emit", "emit"),
            ("builtin-env", "env"),
            ("builtin-str", "str"),
            ("builtin-split", "split"),
            ("builtin-trim", "trim"),
            ("builtin-str-length", "str-length"),
            ("builtin-str-slice", "str-slice"),
            ("builtin-to-int", "to-int"),
            ("builtin-append", "append"),
            ("builtin-length", "length"),
            // docgen-conformance: list-dir, load, expand exported from prelude
            ("builtin-list-dir", "list-dir"),
            ("builtin-load", "load"),
            ("builtin-expand", "expand"),
            ("builtin-eval", "eval"),
            ("builtin-eval-types", "eval-types"),
            ("builtin-blake3", "blake3"),
            ("builtin-cap-identity", "cap-identity"),
            ("builtin-include-cache-get", "include-cache-get"),
            ("builtin-include-cache-put", "include-cache-put"),
            // builtin-privacy-operators-and-io sprint: new builtin-* -> bare-name mappings
            ("builtin-replace", "replace"),
            ("builtin-str-chars", "str-chars"),
            ("builtin-char-code", "char-code"),
            ("builtin-chr", "chr"),
            ("builtin-str-bytes", "str-bytes"),
            ("builtin-bytes-str", "bytes-str"),
            ("builtin-str-index-of", "str-index-of"),
            ("builtin-trim-start", "trim-start"),
            ("builtin-trim-end", "trim-end"),
            ("builtin-str-to-upper-char", "str-to-upper-char"),
            ("builtin-str-to-lower-char", "str-to-lower-char"),
            ("builtin-str-map-chars", "str-map-chars"),
            ("builtin-regex-match?", "regex-match?"),
            // math functions (pow, sqrt, sin, etc.) are NOT injected here:
            // they are stdlib/math.llt exports (require [include %libdir "math.llt"]).
            ("builtin-band", "band"),
            ("builtin-bor", "bor"),
            ("builtin-bxor", "bxor"),
            ("builtin-shl", "shl"),
            ("builtin-shr", "shr"),
            ("builtin-float", "float"),
            // B-168: I/O and builder builtins renamed to builtin-* prefix
            ("builtin-open", "open"),
            ("builtin-write", "write"),
            ("builtin-write-atomic", "write-atomic"),
            ("builtin-write-handle", "write-handle"),
            ("builtin-flush", "flush"),
            ("builtin-close", "close"),
            ("builtin-stat", "stat"),
            ("builtin-exists", "exists"),
            ("builtin-stat-symlink", "stat-symlink"),
            ("builtin-copy-file", "copy-file"),
            ("builtin-symlink", "symlink"),
            ("builtin-set-permissions", "set-permissions"),
            ("builtin-make-dir", "make-dir"),
            ("builtin-rename", "rename"),
            ("builtin-link", "link"),
            ("builtin-read-link", "read-link"),
            ("builtin-get-xattr", "get-xattr"),
            ("builtin-set-xattr", "set-xattr"),
            ("builtin-remove-xattr", "remove-xattr"),
            ("builtin-list-xattrs", "list-xattrs"),
            ("builtin-raw-create", "raw-create"),
            ("builtin-seek", "seek"),
            ("builtin-seek-end", "seek-end"),
            ("builtin-position", "position"),
            ("builtin-revocable", "revocable"),
            ("builtin-revoke-cap", "revoke-cap"),
            ("builtin-cap-data", "cap-data"),
            ("builtin-connect", "connect"),
            ("builtin-tls-layer", "tls-layer"),
            ("builtin-tls-peer-cert", "tls-peer-cert"),
            ("builtin-send-datagram", "send-datagram"),
            ("builtin-recv-datagram", "recv-datagram"),
            ("builtin-string-handle", "string-handle"),
            ("builtin-make-builder", "make-builder"),
            ("builtin-builder-set", "builder-set"),
            ("builtin-builder-delete", "builder-delete"),
            ("builtin-builder-finish", "builder-finish"),
            ("builtin-builder-snapshot", "builder-snapshot"),
            ("builtin-builder-has?", "builder-has?"),
            ("builtin-builder-get", "builder-get"),
            ("builtin-builder-get-or", "builder-get-or"),
            // Reactive cells (T-831)
            ("builtin-reactive-cell", "reactive-cell"),
            ("builtin-cell-get", "cell-get"),
            ("builtin-cell-set", "cell-set"),
        ] {
            if let Some(scheme) = self.get_scheme(canonical) {
                // Builtin aliases are not source-order dict entries; use name-only insertion.
                self.insert_scheme_named_only(alias.to_string(), scheme);
            }
        }
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env")
            .field("slots", &self.slots.len())
            .field("extras", &self.extras.len())
            .field("classes", &self.classes.len())
            .field("instances", &self.instances.len())
            .field("type_aliases", &self.type_aliases.len())
            .field("has_parent", &self.parent.is_some())
            .finish()
    }
}
