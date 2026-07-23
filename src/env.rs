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
use crate::types::{Type, TypeScheme};

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
            slots: IndexMap::new(),
            extras: HashMap::new(),
            classes: IndexMap::new(),
            instances: IndexMap::new(),
            tycon_defs: HashMap::new(),
            parent: None,
        }
    }

    /// Insert a name-only typed entry (no de Bruijn slot) for an injected runtime value.
    /// Used by the CLI to register capability types (%programs, %cwd, etc.) so the
    /// type-checker can see them. The `ty` is wrapped in a monomorphic TypeScheme.
    pub fn insert_injected(&mut self, name: String, ty: crate::types::Type) {
        self.extras.insert(
            name,
            EnvSlot {
                scheme: Some(crate::types::TypeScheme {
                    type_vars: vec![],
                    constraints: vec![],
                    body: ty,
                    label_vars: vec![],
                    kind_vars: vec![],
                    doc: None,
                    inner_schemes: None,
                    param_narrowings: Vec::new(),
                }),
            },
        );
    }

    /// Create an empty environment with the given parent.
    pub fn with_parent(parent: Arc<RwLock<Env>>) -> Self {
        Self {
            slots: IndexMap::new(),
            extras: HashMap::new(),
            classes: IndexMap::new(),
            instances: IndexMap::new(),
            tycon_defs: HashMap::new(),
            parent: Some(parent),
        }
    }

    /// Return the keys of the slots IndexMap as a `Vec<String>`.
    ///
    /// Used by tests to verify that builtins and type declarations are registered.
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
    /// slot position (see `EvalContext::new_scope_arena`).
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
            self.slots.insert(
                name,
                EnvSlot {
                    scheme: Some(scheme),
                },
            );
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
            self.extras.insert(
                name,
                EnvSlot {
                    scheme: Some(scheme),
                },
            );
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
    for frame_arc in frames.iter() {
        let frame = frame_arc.read().unwrap();
        for (name, slot) in &frame.slots {
            collected
                .slots
                .entry(name.clone())
                .or_insert_with(|| slot.clone());
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
    for (name, slot) in collected.slots {
        guard.slots.insert(name, slot);
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

    use crate::type_def::{TyConDef, Type};

    use super::Env;

    fn make_def(name: &str) -> Arc<TyConDef> {
        Arc::new(TyConDef::new_with_body(name, Type::Unknown))
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
        // Setup: dst defines slot "foo"; src (child of dst) also defines slot "foo".
        // After merge_env_chain_into(src, dst), dst's "foo" slot must be the src's
        // slot (later document shadows earlier document, matching runtime right-biased merge).

        // dst has slot "foo" with no scheme (name-only, as inserted by insert_slot_name_only)
        let mut dst_raw = Env::new();
        dst_raw.insert_slot_name_only("foo".to_string());
        let dst = Arc::new(RwLock::new(dst_raw));

        // src (child of dst) also has slot "foo" with a TypeScheme (populated by the type checker)
        let mut src_raw = Env::with_parent(Arc::clone(&dst));
        src_raw.insert_scheme(
            "foo".to_string(),
            crate::types::TypeScheme::mono(crate::type_def::Type::Unknown),
        );
        let src = Arc::new(RwLock::new(src_raw));

        merge_env_chain_into(&src, &dst);

        let guard = dst.read().unwrap();
        let slot = guard
            .slots
            .get("foo")
            .expect("slot 'foo' must be present in dst");
        // After merge, the slot should carry the src's scheme (Some), not the dst's None.
        assert!(
            slot.scheme.is_some(),
            "src's slot (with scheme) must shadow dst's slot (name-only, scheme=None) \
             after merge_env_chain_into"
        );
    }
}
