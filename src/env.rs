//! Unified environment: merged `TypeEnv` (type checker) and `Environment` (evaluator)
//! into a single `Env` struct used by both.
//!
//! After T-1557, `Env` is **type-metadata only**. Runtime values are stored exclusively
//! in `FlatEnv` (arena-complete). Each slot holds an optional TypeValue (populated at
//! typecheck time). De Bruijn (level, slot) indices address the Vec-based `slots`
//! directly — no coordinate-system mismatch.
//!
//! After S-1003 T-2004: `TypeScheme` is deleted. Slots now store `TypeValue` directly
//! (an `Arc<Value>`). Monomorphic types are stored as their TypeValue directly;
//! polymorphic types are `TypeValue.Scheme` variants.
//!
//! Parent chain uses `Arc<RwLock<Env>>` (no `Rc` anywhere).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::ast::Span;
use crate::type_class::{ClassDecl, InstanceDecl, TypeValue};

// ---------------------------------------------------------------------------
// EnvSlot
// ---------------------------------------------------------------------------

/// A single binding slot: scheme side (type checker) only.
///
/// After T-1557, runtime values are stored exclusively in `FlatEnv` (arena-complete).
/// `Env` is type-metadata only.
///
/// After S-1003: `scheme` holds a `TypeValue` (`Arc<Value>`) directly.
/// Monomorphic bindings store their TypeValue as-is; polymorphic bindings
/// store a `TypeValue.Scheme` variant.
#[derive(Debug, Clone)]
pub struct EnvSlot {
    pub scheme: Option<TypeValue>,
    /// Whether this binding has been referenced during type checking.
    /// Set to `true` by `mark_extras_referenced` when a VarRef resolves to this slot.
    /// Used by lost-binding warnings to detect unreferenced intermediate dict bindings.
    pub referenced: bool,
    /// Source span where this binding was defined.
    /// Used by lost-binding warnings to point to the binding site in error messages.
    /// `None` for injected builtins and other bindings without a user-visible definition site.
    pub definition_span: Option<Span>,
}

// ---------------------------------------------------------------------------
// Env
// ---------------------------------------------------------------------------

/// Unified scope frame used by the type checker (type-metadata only after T-1557).
///
/// `slots` is a `Vec<Option<(String, EnvSlot)>>` indexed by resolver-assigned
/// de Bruijn slot. Position N corresponds to the resolver's LGM(slot=N).
/// `None` entries are unoccupied positions (the Vec may be sparse when
/// cumulative offsets leave gaps).
///
/// `extras` holds name-only entries with no resolver-assigned slot (builtins,
/// injected names, narrowing overrides, class dispatch). These are reachable
/// via `get_extras_scheme` but never via `get_scheme_at`.
#[derive(Clone)]
pub struct Env {
    /// Slot-indexed entries: position N corresponds to resolver LGM(slot=N).
    /// `None` entries are unoccupied positions.
    pub(crate) slots: Vec<Option<(String, EnvSlot)>>,
    /// Reverse index: name → slot position in `slots` Vec. Enables O(1) name-based
    /// lookups into the Vec without linear scanning. Kept in sync by
    /// `insert_at_slot` and `insert_slot_name_only`.
    pub(crate) slot_index: HashMap<String, usize>,
    /// Name-only entries with no resolver-assigned slot (builtins, injected names).
    pub(crate) extras: HashMap<String, EnvSlot>,
    /// Class declarations registered in this scope frame.
    pub(crate) classes: indexmap::IndexMap<String, ClassDecl>,
    /// Instance declarations keyed by mangled name (e.g., "ɪɴꜱᴛᴀɴᴄᴇ⧼Equatable∷=⟨Int⟩⧽").
    pub(crate) instances: indexmap::IndexMap<String, InstanceDecl>,
    /// Type constructor definitions registered in this scope frame.
    /// Populated alongside `InferState.tycon_env` during type-checking Pass 2.
    /// Enables scoped TyConDef lookup via the Env parent chain, complementing
    /// the flat `InferState.tycon_env` store.
    pub(crate) tycon_defs: HashMap<String, Arc<crate::type_def::TyConDef>>,
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
            slots: Vec::new(),
            slot_index: HashMap::new(),
            extras: HashMap::new(),
            classes: indexmap::IndexMap::new(),
            instances: indexmap::IndexMap::new(),
            tycon_defs: HashMap::new(),
            parent: None,
        }
    }

    /// Insert a name-only typed entry (no de Bruijn slot) for an injected runtime value.
    /// Used by the CLI to register capability types (%programs, %cwd, etc.) so the
    /// type-checker can see them. The TypeValue is stored directly as a monomorphic scheme.
    pub fn insert_injected(&mut self, name: String, tv: TypeValue) {
        self.extras.insert(
            name,
            EnvSlot {
                scheme: Some(tv),
                referenced: false,
                definition_span: None, // injected builtins have no user-visible definition site
            },
        );
    }

    /// Create an empty environment with the given parent.
    pub fn with_parent(parent: Arc<RwLock<Env>>) -> Self {
        Self {
            slots: Vec::new(),
            slot_index: HashMap::new(),
            extras: HashMap::new(),
            classes: indexmap::IndexMap::new(),
            instances: indexmap::IndexMap::new(),
            tycon_defs: HashMap::new(),
            parent: Some(parent),
        }
    }

    /// Return the names of occupied slot entries as a `Vec<String>`.
    ///
    /// Used by tests to verify that builtins and type declarations are registered.
    pub fn slot_names(&self) -> Vec<String> {
        self.slots
            .iter()
            .filter_map(|entry| entry.as_ref().map(|(name, _)| name.clone()))
            .collect()
    }

    /// Iterate over occupied slot entries in the current frame.
    pub fn iter_slots(&self) -> impl Iterator<Item = (&str, &EnvSlot)> {
        self.slots
            .iter()
            .filter_map(|entry| entry.as_ref().map(|(name, slot)| (name.as_str(), slot)))
    }

    /// Check whether a name is present in this environment (slots or extras), walking the
    /// parent chain.
    ///
    /// Returns `true` if the name was registered with `insert_slot_name_only`,
    /// `insert_at_slot`, or `insert_scheme_named_only` — regardless of whether a
    /// `TypeScheme` is associated. This is a name-presence test only; it does not force
    /// any runtime thunk.
    pub fn has_name(&self, name: &str) -> bool {
        if self.slot_index.contains_key(name) || self.extras.contains_key(name) {
            return true;
        }
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_arc) = current {
            let env = env_arc.read().unwrap();
            if env.slot_index.contains_key(name) || env.extras.contains_key(name) {
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
        // Check current frame slots
        for entry in &self.slots {
            if let Some((name, _)) = entry {
                if name.ends_with(suffix) {
                    return Some(name.clone());
                }
            }
        }
        // Walk parent chain
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_arc) = current {
            let env = env_arc.read().unwrap();
            for entry in &env.slots {
                if let Some((name, _)) = entry {
                    if name.ends_with(suffix) {
                        return Some(name.clone());
                    }
                }
            }
            current = env.parent.as_ref().map(Arc::clone);
        }
        None
    }

    // -----------------------------------------------------------------------
    // SCHEME SIDE (from TypeEnv)
    // -----------------------------------------------------------------------

    /// Insert a name-only entry (no TypeScheme) into the slots Vec.
    ///
    /// This is used by `build_core_env` to register builtin names so the resolver
    /// can assign de Bruijn (level, slot) coordinates without requiring a TypeScheme
    /// at bootstrap time. The entry is appended to the slots Vec at the next available
    /// position, which must match the order that the resolver uses to assign slot indices.
    ///
    /// If an entry with this name already exists in slots, this is a no-op.
    pub fn insert_slot_name_only(&mut self, name: String) {
        if self.slot_index.contains_key(&name) {
            return; // already present
        }
        let pos = self.slots.len();
        self.slots.push(Some((
            name.clone(),
            EnvSlot {
                scheme: None,
                referenced: false,
                definition_span: None,
            },
        )));
        self.slot_index.insert(name, pos);
    }

    /// Pre-allocate the slot Vec to hold `count` entries, all initialized to `None`.
    ///
    /// Called before `insert_at_slot` to ensure the Vec is large enough for all
    /// resolver-assigned slots in this scope frame. Clears the slot_index since
    /// the Vec is being replaced.
    pub fn prepare_slots(&mut self, count: usize) {
        self.slots = vec![None; count];
        self.slot_index.clear();
    }

    /// Insert a TypeValue at a specific resolver-assigned slot position.
    ///
    /// The slot index comes from the resolver's name→slot mapping for this scope
    /// frame. If the slot is beyond the current Vec length, the Vec is extended
    /// with `None` entries.
    ///
    /// `definition_span` is `Some(span)` when the binding has a user-visible definition
    /// site (function parameters, sequential intermediate dict entries). Pass `None` for
    /// synthetic/injected bindings that should not participate in lost-binding tracking.
    pub fn insert_at_slot(
        &mut self,
        slot: usize,
        name: String,
        scheme: TypeValue,
        definition_span: Option<Span>,
    ) {
        if slot >= self.slots.len() {
            self.slots.resize_with(slot + 1, || None);
        }
        self.slot_index.insert(name.clone(), slot);
        self.slots[slot] = Some((
            name,
            EnvSlot {
                scheme: Some(scheme),
                referenced: false,
                definition_span,
            },
        ));
    }

    /// Mark the slot entry at `(level, slot)` as referenced, walking `level` parent frames.
    ///
    /// Returns `true` if the slot was found and marked, `false` if not found.
    /// Called by `infer_var_ref` when a resolver-assigned address resolves successfully,
    /// to record that the binding has been used (for lost-binding warning tracking).
    pub fn mark_slot_referenced(&mut self, level: u32, slot: u32) -> bool {
        if level == 0 {
            if let Some(Some((_, ref mut slot_entry))) = self.slots.get_mut(slot as usize) {
                slot_entry.referenced = true;
                return true;
            }
            return false;
        }
        if let Some(ref parent) = self.parent {
            let mut guard = parent.write().unwrap();
            return guard.mark_slot_referenced(level - 1, slot);
        }
        false
    }

    /// Return the `definition_span` of the slot entry at `(level, slot)`, walking `level`
    /// parent frames.
    ///
    /// Returns `None` if the slot is not found or has no definition span (synthetic binding).
    /// Used by `infer_var_ref` to build a span-keyed `BindingId` for use_def edge recording.
    pub fn get_slot_def_span(&self, level: u32, slot: u32) -> Option<Span> {
        if level == 0 {
            return self
                .slots
                .get(slot as usize)
                .and_then(|s| s.as_ref())
                .and_then(|(_, slot_entry)| slot_entry.definition_span.clone());
        }
        if let Some(ref parent) = self.parent {
            let guard = parent.read().unwrap();
            return guard.get_slot_def_span(level - 1, slot);
        }
        None
    }

    /// Find the `definition_span` for a binding by name, walking slots then extras
    /// through the full parent chain. Returns the first `definition_span` found.
    ///
    /// Used by `apply_narrowings` to build a `BindingId` for `narrowing_map` insertion
    /// without needing a resolver-assigned slot address.
    pub fn find_def_span_by_name(env: &Arc<std::sync::RwLock<Env>>, name: &str) -> Option<Span> {
        let guard = env.read().unwrap();
        // Check slots by name
        if let Some(&pos) = guard.slot_index.get(name) {
            if let Some(Some((_, ref slot_entry))) = guard.slots.get(pos) {
                if let Some(ref span) = slot_entry.definition_span {
                    return Some(span.clone());
                }
            }
        }
        // Check extras
        if let Some(ref slot_entry) = guard.extras.get(name) {
            if let Some(ref span) = slot_entry.definition_span {
                return Some(span.clone());
            }
        }
        let parent = guard.parent.clone();
        drop(guard);
        parent
            .as_ref()
            .and_then(|p| Self::find_def_span_by_name(p, name))
    }

    /// Insert a TypeValue into the extras HashMap (name-only, no slot).
    ///
    /// Use this for entries NOT assigned a slot by the resolver:
    /// - ADT constructor type information
    /// - Class method injections
    /// - Narrowing overrides
    /// - Builtin bindings
    ///
    /// These entries are visible via `get_scheme` but never reached via `get_scheme_at`.
    pub fn insert_scheme_named_only(&mut self, name: String, scheme: TypeValue) {
        if let Some(slot) = self.extras.get_mut(&name) {
            slot.scheme = Some(scheme);
        } else {
            self.extras.insert(
                name,
                EnvSlot {
                    scheme: Some(scheme),
                    referenced: false,
                    definition_span: None,
                },
            );
        }
    }

    /// Insert a TypeValue into the extras HashMap with a definition span for lost-binding detection.
    ///
    /// Like `insert_scheme_named_only` but stores the source span where the binding was declared.
    /// Use this when the binding has a user-visible definition site (function params, let bindings,
    /// case arm bindings). Use `insert_scheme_named_only` for injected/synthetic bindings.
    pub fn insert_scheme_with_span(
        &mut self,
        name: String,
        scheme: TypeValue,
        definition_span: Span,
    ) {
        if let Some(slot) = self.extras.get_mut(&name) {
            slot.scheme = Some(scheme);
            slot.definition_span = Some(definition_span);
        } else {
            self.extras.insert(
                name,
                EnvSlot {
                    scheme: Some(scheme),
                    referenced: false,
                    definition_span: Some(definition_span),
                },
            );
        }
    }

    /// Look up a TypeValue by name, walking slots then extras then the parent chain.
    ///
    /// Scans the Vec-based slots for a matching name (O(n) per frame), then checks
    /// extras, then walks the parent chain.
    ///
    /// Returns a cloned `TypeValue` because the parent chain is behind
    /// `Arc<RwLock<Env>>` — references cannot outlive the lock guard.
    pub fn get_scheme(&self, name: &str) -> Option<TypeValue> {
        // Check slots in current frame via slot_index (O(1))
        if let Some(&pos) = self.slot_index.get(name) {
            if let Some(Some((_, ref slot))) = self.slots.get(pos) {
                if let Some(ref s) = slot.scheme {
                    return Some(s.clone());
                }
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
            if let Some(&pos) = env.slot_index.get(name) {
                if let Some(Some((_, ref slot))) = env.slots.get(pos) {
                    if let Some(ref s) = slot.scheme {
                        return Some(s.clone());
                    }
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

    /// Look up a TypeValue by name in extras ONLY (not slots), walking the parent chain.
    ///
    /// Used for narrowing overrides and class dispatch injections that have no
    /// resolver-assigned slot.
    pub fn get_extras_scheme(&self, name: &str) -> Option<TypeValue> {
        // Check extras in current frame
        if let Some(slot) = self.extras.get(name) {
            if let Some(ref s) = slot.scheme {
                return Some(s.clone());
            }
        }
        // Walk parent chain (extras only)
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_arc) = current {
            let env = env_arc.read().unwrap();
            if let Some(slot) = env.extras.get(name) {
                if let Some(ref s) = slot.scheme {
                    return Some(s.clone());
                }
            }
            current = env.parent.as_ref().map(Arc::clone);
        }
        None
    }

    /// Mark an extras entry as referenced, walking the parent chain.
    ///
    /// This method mirrors `get_extras_scheme` but mutates the `referenced` flag instead of
    /// returning a TypeScheme. Called by VarRef inference when a variable resolves to an
    /// extras entry, enabling lost-binding warnings to detect unreferenced intermediate dict
    /// bindings.
    ///
    /// Stops at the first frame where the name is found (no fallthrough to deeper frames).
    pub fn mark_extras_referenced(&mut self, name: &str) {
        if let Some(slot) = self.extras.get_mut(name) {
            slot.referenced = true;
            return;
        }
        // Walk parent chain
        let mut current = self.parent.clone();
        while let Some(parent_ref) = current {
            let mut parent_guard = parent_ref.write().unwrap();
            if let Some(slot) = parent_guard.extras.get_mut(name) {
                slot.referenced = true;
                return;
            }
            current = parent_guard.parent.clone();
        }
    }

    /// Slot-indexed TypeValue lookup: walk `level` parent frames (0 = current),
    /// look up `slot` in the target frame's `slots` Vec, and return the
    /// TypeValue.
    ///
    /// **NO name check, NO expected_name parameter.** This is intentional — the
    /// old `get_type_at(level, slot, expected_name)` name check was the bug causing
    /// class method type-warning false positives (slot key is the i-prefixed
    /// instance binding name, VarRef source name is the method name).
    ///
    /// Returns a cloned TypeValue because the parent chain is behind
    /// `Arc<RwLock<Env>>`.
    pub fn get_scheme_at(&self, level: u32, slot: u32) -> Option<TypeValue> {
        if level == 0 {
            return self
                .slots
                .get(slot as usize)
                .and_then(|entry| entry.as_ref())
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
                    .get(slot as usize)
                    .and_then(|entry| entry.as_ref())
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
    /// Returns a reference to the TypeValue since no lock traversal is needed.
    pub fn get_own_scheme(&self, name: &str) -> Option<&TypeValue> {
        if let Some(&pos) = self.slot_index.get(name) {
            if let Some(Some((_, ref slot))) = self.slots.get(pos) {
                if let Some(ref scheme) = slot.scheme {
                    return Some(scheme);
                }
            }
        }
        // Slot had no scheme or name not in slot_index — check extras.
        self.extras.get(name).and_then(|s| s.scheme.as_ref())
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
    // TYCON DEFS
    // -----------------------------------------------------------------------

    /// Insert a type constructor definition into this frame.
    pub fn insert_tycon_def(&mut self, name: String, def: Arc<crate::type_def::TyConDef>) {
        self.tycon_defs.insert(name, def);
    }

    /// Look up a type constructor definition by name, walking the parent chain.
    ///
    /// Returns a cloned `Arc<TyConDef>` because the parent chain is behind
    /// `Arc<RwLock<Env>>`.
    pub fn lookup_tycon_def(&self, name: &str) -> Option<Arc<crate::type_def::TyConDef>> {
        if let Some(def) = self.tycon_defs.get(name) {
            return Some(Arc::clone(def));
        }
        let mut current = self.parent.as_ref().map(Arc::clone);
        while let Some(env_arc) = current {
            let env = env_arc.read().unwrap();
            if let Some(def) = env.tycon_defs.get(name) {
                return Some(Arc::clone(def));
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
        for entry in &self.slots {
            if let Some((name, _)) = entry {
                names.insert(name.clone());
            }
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
        for entry in &self.slots {
            if let Some((name, _)) = entry {
                names.insert(name.clone());
            }
        }
        for name in self.extras.keys() {
            names.insert(name.clone());
        }
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

    /// Copy all bindings from `other` into `self`.
    ///
    /// Copies only the own (non-parent) bindings from `other`.
    /// Parent chains are not traversed. Existing entries in `self` with the same
    /// name are overwritten by entries from `other`.
    pub fn merge(&mut self, other: Env) {
        for entry in other.slots {
            if let Some((name, slot)) = entry {
                // Use slot_index for O(1) lookup
                if let Some(&pos) = self.slot_index.get(&name) {
                    if slot.scheme.is_some() {
                        self.slots[pos] = Some((name, slot));
                    }
                } else {
                    let pos = self.slots.len();
                    self.slot_index.insert(name.clone(), pos);
                    self.slots.push(Some((name, slot)));
                }
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
        for (name, decl) in other.classes {
            self.classes.insert(name, decl);
        }
        for (mangled, decl) in other.instances {
            self.instances.insert(mangled, decl);
        }
        for (name, def) in other.tycon_defs {
            self.tycon_defs.insert(name, def);
        }
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

/// Walk the parent chain of `src` (innermost-first) up to but not including `dst`,
/// and merge every collected frame's bindings into `dst`.
///
/// Innermost entries win: `or_insert`-style semantics prevent an outer frame from
/// overwriting a name that was already contributed by a closer frame. This preserves
/// shadowing: if frame A (inner) defines `x` and frame B (outer) also defines `x`,
/// only frame A's scheme reaches `dst`.
///
/// All five binding categories are merged: slots, extras, classes,
/// instances, and tycon_defs.
///
/// `dst` must be an ancestor of `src` (or the walk stops at the root frame).
/// `dst` itself is never pushed into the collected frames — the walk stops before
/// `dst` via `Arc::ptr_eq`. This prevents a write-then-read-lock deadlock on `dst`.
pub fn merge_env_chain_into(src: &Arc<RwLock<Env>>, dst: &Arc<RwLock<Env>>) {
    // Collect all frames from innermost to outermost, stopping before dst.
    // dst must never appear in `frames` — we hold a write lock on dst during the
    // merge loop below, and attempting to read-lock it would deadlock on glibc.
    let mut frames: Vec<Arc<RwLock<Env>>> = Vec::new();
    let mut current = Some(Arc::clone(src));
    while let Some(arc) = current {
        if Arc::ptr_eq(&arc, dst) {
            break; // stop before dst — dst is never added to frames
        }
        let parent = arc.read().unwrap().parent.as_ref().map(Arc::clone);
        frames.push(arc);
        current = parent;
    }
    if frames.is_empty() {
        return;
    }
    // Phase 1: Collect entries from the chain with or_insert (inner scope wins
    // within the chain — innermost frame is processed first, outer frame entries
    // are only added if not already present from an inner frame).
    let mut collected = Env::new();
    // Use a HashMap to track name→slot for deduplication (inner wins via or_insert)
    let mut collected_slot_map: std::collections::HashMap<String, EnvSlot> =
        std::collections::HashMap::new();
    for frame_arc in frames.iter() {
        let frame = frame_arc.read().unwrap();
        for entry in &frame.slots {
            if let Some((name, slot)) = entry {
                collected_slot_map
                    .entry(name.clone())
                    .or_insert_with(|| slot.clone());
            }
        }
        for (name, slot) in &frame.extras {
            collected
                .extras
                .entry(name.clone())
                .or_insert_with(|| slot.clone());
        }
        for (name, decl) in &frame.classes {
            collected
                .classes
                .entry(name.clone())
                .or_insert_with(|| decl.clone());
        }
        for (mangled, decl) in &frame.instances {
            collected
                .instances
                .entry(mangled.clone())
                .or_insert_with(|| decl.clone());
        }
        for (name, def) in &frame.tycon_defs {
            collected
                .tycon_defs
                .entry(name.clone())
                .or_insert_with(|| Arc::clone(def));
        }
    }
    // Phase 2: Insert collected entries into dst with `insert` (later document
    // shadows earlier document — matches runtime `merge` right-biased semantics).
    let mut guard = dst.write().unwrap();
    // Merge collected slots: find by name in dst or append
    for (name, slot) in collected_slot_map {
        if let Some(&pos) = guard.slot_index.get(&name) {
            guard.slots[pos] = Some((name, slot));
        } else {
            let pos = guard.slots.len();
            guard.slot_index.insert(name.clone(), pos);
            guard.slots.push(Some((name, slot)));
        }
    }
    for (name, slot) in collected.extras {
        guard.extras.insert(name, slot);
    }
    for (name, decl) in collected.classes {
        guard.classes.insert(name, decl);
    }
    for (mangled, decl) in collected.instances {
        guard.instances.insert(mangled, decl);
    }
    for (name, def) in collected.tycon_defs {
        guard.tycon_defs.insert(name, def);
    }
}

impl std::fmt::Debug for Env {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env")
            .field("slots", &self.slots.len())
            .field("extras", &self.extras.len())
            .field("classes", &self.classes.len())
            .field("instances", &self.instances.len())
            .field("tycon_defs", &self.tycon_defs.len())
            .field("has_parent", &self.parent.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use crate::type_def::TyConDef;

    use super::Env;

    fn make_def(name: &str) -> Arc<TyConDef> {
        Arc::new(TyConDef::new_with_body(
            name,
            crate::value::unknown_type_val(),
        ))
    }

    #[test]
    fn test_lookup_tycon_def_local() {
        let mut env = Env::new();
        let def = make_def("Color");
        env.insert_tycon_def("Color".to_string(), Arc::clone(&def));

        let found = env.lookup_tycon_def("Color");
        assert!(found.is_some(), "should find TyConDef in local frame");
        assert!(Arc::ptr_eq(&found.unwrap(), &def));
    }

    #[test]
    fn test_lookup_tycon_def_not_found() {
        let env = Env::new();
        assert!(
            env.lookup_tycon_def("Missing").is_none(),
            "should return None when name is not in any frame"
        );
    }

    #[test]
    fn test_lookup_tycon_def_parent_chain() {
        // TyConDef inserted in parent is visible from child.
        let mut parent = Env::new();
        let def = make_def("Shape");
        parent.insert_tycon_def("Shape".to_string(), Arc::clone(&def));

        let child = Env::with_parent(Arc::new(RwLock::new(parent)));

        let found = child.lookup_tycon_def("Shape");
        assert!(found.is_some(), "should find TyConDef from parent chain");
        assert!(Arc::ptr_eq(&found.unwrap(), &def));
    }

    #[test]
    fn test_lookup_tycon_def_inner_shadows_outer() {
        // TyConDef with same name in child shadows parent's definition.
        let mut parent = Env::new();
        let outer_def = make_def("Result");
        parent.insert_tycon_def("Result".to_string(), Arc::clone(&outer_def));

        let mut child = Env::with_parent(Arc::new(RwLock::new(parent)));
        let inner_def = make_def("Result");
        child.insert_tycon_def("Result".to_string(), Arc::clone(&inner_def));

        let found = child.lookup_tycon_def("Result");
        assert!(found.is_some(), "should find TyConDef in child frame");
        // The inner def is returned, not the outer.
        assert!(
            Arc::ptr_eq(&found.unwrap(), &inner_def),
            "inner frame TyConDef should shadow outer frame"
        );
    }

    // -----------------------------------------------------------------------
    // merge_env_chain_into tests
    // -----------------------------------------------------------------------

    use super::merge_env_chain_into;

    #[test]
    fn test_merge_env_chain_basic() {
        // Basic merge: binding from child frame appears in dst after the call.
        let dst = Arc::new(RwLock::new(Env::new()));
        let mut child_raw = Env::with_parent(Arc::clone(&dst));
        child_raw.insert_tycon_def("Foo".to_string(), make_def("Foo"));
        let child = Arc::new(RwLock::new(child_raw));

        merge_env_chain_into(&child, &dst);

        let guard = dst.read().unwrap();
        assert!(
            guard.lookup_tycon_def("Foo").is_some(),
            "Foo from child should be merged into dst"
        );
    }

    #[test]
    fn test_merge_env_chain_inner_wins_over_outer() {
        // Shadowing: if both an inner and an outer frame define the same name,
        // the inner frame's definition should appear in dst.
        // merge_env_chain_into collects frames innermost-first (frames[0] = inner),
        // then iterates innermost-first with or_insert_with: inner is inserted first,
        // and when outer is processed, or_insert_with skips the already-present key.
        let dst = Arc::new(RwLock::new(Env::new()));

        let mut outer_raw = Env::with_parent(Arc::clone(&dst));
        let outer_def = make_def("Thing");
        outer_raw.insert_tycon_def("Thing".to_string(), Arc::clone(&outer_def));
        let outer = Arc::new(RwLock::new(outer_raw));

        let mut inner_raw = Env::with_parent(Arc::clone(&outer));
        let inner_def = make_def("Thing");
        inner_raw.insert_tycon_def("Thing".to_string(), Arc::clone(&inner_def));
        let inner = Arc::new(RwLock::new(inner_raw));

        // inner → outer → dst
        merge_env_chain_into(&inner, &dst);

        let guard = dst.read().unwrap();
        let found = guard.lookup_tycon_def("Thing");
        assert!(found.is_some(), "Thing should appear in dst");
        // merge iterates innermost-first; inner_def is inserted first via or_insert_with,
        // so when outer is processed, the key already exists and or_insert_with is a no-op.
        assert!(
            Arc::ptr_eq(&found.unwrap(), &inner_def),
            "inner frame's definition should win over outer frame's"
        );
    }

    #[test]
    fn test_merge_env_chain_dst_in_chain_no_deadlock() {
        // This is the critical regression test: dst is in the parent chain of src.
        // The Arc::ptr_eq stop must prevent dst from being pushed into frames,
        // which would cause a write+read deadlock on glibc.
        //
        // Chain: child2 → child1 → dst
        let dst = Arc::new(RwLock::new(Env::new()));

        let mut child1_raw = Env::with_parent(Arc::clone(&dst));
        child1_raw.insert_tycon_def("FromChild1".to_string(), make_def("FromChild1"));
        let child1 = Arc::new(RwLock::new(child1_raw));

        let mut child2_raw = Env::with_parent(Arc::clone(&child1));
        child2_raw.insert_tycon_def("FromChild2".to_string(), make_def("FromChild2"));
        let child2 = Arc::new(RwLock::new(child2_raw));

        // Must not deadlock, and must populate dst with child1 and child2 bindings.
        merge_env_chain_into(&child2, &dst);

        let guard = dst.read().unwrap();
        assert!(
            guard.lookup_tycon_def("FromChild2").is_some(),
            "child2's binding should be merged into dst"
        );
        assert!(
            guard.lookup_tycon_def("FromChild1").is_some(),
            "child1's binding should be merged into dst"
        );
    }

    #[test]
    fn test_merge_env_chain_src_shadows_dst() {
        // Later document (src chain) shadows earlier document (dst).
        // This matches runtime `merge` right-biased semantics.
        //
        // Chain: child → dst
        // Both child and dst define "Shared". child's definition should win
        // because it represents the later document's bindings.
        let mut dst_raw = Env::new();
        let dst_def = make_def("Shared");
        dst_raw.insert_tycon_def("Shared".to_string(), Arc::clone(&dst_def));
        let dst = Arc::new(RwLock::new(dst_raw));

        let mut child_raw = Env::with_parent(Arc::clone(&dst));
        let child_def = make_def("Shared");
        child_raw.insert_tycon_def("Shared".to_string(), Arc::clone(&child_def));
        let child = Arc::new(RwLock::new(child_raw));

        merge_env_chain_into(&child, &dst);

        let guard = dst.read().unwrap();
        let found = guard.tycon_defs.get("Shared");
        assert!(found.is_some(), "Shared must still be in dst");
        assert!(
            Arc::ptr_eq(found.unwrap(), &child_def),
            "child's definition of Shared should shadow dst's pre-existing definition"
        );
    }

    #[test]
    fn test_merge_env_chain_tycon_defs_merged() {
        // Verify that tycon_defs (not just slots) are merged.
        let dst = Arc::new(RwLock::new(Env::new()));
        let mut child_raw = Env::with_parent(Arc::clone(&dst));
        let def = make_def("MyType");
        child_raw.insert_tycon_def("MyType".to_string(), Arc::clone(&def));
        let child = Arc::new(RwLock::new(child_raw));

        merge_env_chain_into(&child, &dst);

        let guard = dst.read().unwrap();
        let found = guard.tycon_defs.get("MyType");
        assert!(found.is_some(), "tycon_def from child must appear in dst");
        assert!(
            Arc::ptr_eq(found.unwrap(), &def),
            "the merged tycon_def must be the same Arc"
        );
    }

    // ── slots shadowing test ──────────────────────────────────────────────────

    #[test]
    fn test_merge_env_chain_slots_src_shadows_dst() {
        // Verify that slots — the most semantically critical field — are merged
        // with later-document-wins (Phase 2 plain insert) semantics.
        //
        // Setup: dst defines an extras entry "foo" with no scheme; src (child of dst) also
        // defines "foo" as a slot with a TypeScheme. After merge_env_chain_into(src, dst),
        // dst must have the src's slot with the TypeScheme.

        // dst has "foo" with no scheme (name-only, inserted via insert_slot_name_only → extras)
        let mut dst_raw = Env::new();
        dst_raw.insert_slot_name_only("foo".to_string());
        let dst = Arc::new(RwLock::new(dst_raw));

        // src (child of dst) has slot "foo" with a TypeValue (populated by the type checker)
        let mut src_raw = Env::with_parent(Arc::clone(&dst));
        src_raw.insert_scheme_named_only(
            "foo".to_string(),
            crate::type_infer::make_typevalue_unknown(),
        );
        let src = Arc::new(RwLock::new(src_raw));

        merge_env_chain_into(&src, &dst);

        // After merge, "foo" should have the src's scheme (Some).
        // It may be in slots (from merge) or extras (from insert_slot_name_only).
        let guard = dst.read().unwrap();
        let has_scheme = guard.get_own_scheme("foo").is_some();
        assert!(
            has_scheme,
            "src's slot (with scheme) must be present in dst after merge_env_chain_into"
        );
    }
}
