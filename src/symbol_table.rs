use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use indexmap::IndexMap;

use crate::ast::Span;
use crate::value::{Thunk, Value};

/// Which pipeline stage a scope frame's bindings belong to. Resolution searches
/// the single unified scope chain filtered by stage: type expressions search
/// `Type` frames first then `Runtime` frames, ordinary runtime references search
/// only `Runtime` frames.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Type,
    Runtime,
}

const STAGE_RUNTIME: u8 = 0;
const STAGE_TYPE: u8 = 1;

pub struct EvidenceEntry {
    pub handler_frame: Arc<SymbolTable>,
    pub method_slots: Vec<u32>,
}

pub struct SymbolEntry {
    pub slot: u32,
    pub type_scheme: OnceLock<Arc<Value>>,
    pub value: OnceLock<Arc<Thunk>>,
    pub referenced: AtomicBool,
    pub definition_span: Span,
}

pub struct SymbolTable {
    entries: IndexMap<String, SymbolEntry>,
    evidence: OnceLock<HashMap<u64, EvidenceEntry>>,
    type_instantiation: Option<HashMap<String, Arc<Value>>>,
    stage: AtomicU8,
}

pub type ScopeChain = Vec<Arc<SymbolTable>>;

impl SymbolTable {
    pub fn new() -> Self {
        Self {
            entries: IndexMap::new(),
            evidence: OnceLock::new(),
            type_instantiation: None,
            stage: AtomicU8::new(STAGE_RUNTIME),
        }
    }

    pub fn new_with_type_inst(type_inst: Option<HashMap<String, Arc<Value>>>) -> Self {
        Self {
            entries: IndexMap::new(),
            evidence: OnceLock::new(),
            type_instantiation: type_inst,
            stage: AtomicU8::new(STAGE_RUNTIME),
        }
    }

    /// Mark this frame's bindings as belonging to the type stage (produced by a
    /// `--- stage: "type"` document's own top-level body). Called exactly once,
    /// from `SurfaceResolver::walk_surface_document`, after the frame has already
    /// been pushed onto the scope stack (frames are shared via `Arc` by then, so
    /// this uses interior mutability rather than rebuilding the table).
    pub fn mark_type_stage(&self) {
        self.stage.store(STAGE_TYPE, Ordering::Relaxed);
    }

    /// Which stage this frame's bindings belong to. Defaults to `Runtime`.
    pub fn stage(&self) -> Stage {
        match self.stage.load(Ordering::Relaxed) {
            STAGE_TYPE => Stage::Type,
            _ => Stage::Runtime,
        }
    }

    pub fn empty() -> Arc<Self> {
        Arc::new(Self::new())
    }

    pub fn empty_chain() -> ScopeChain {
        vec![Self::empty()]
    }

    pub fn insert(&mut self, name: String, slot: u32, def_span: Span) {
        self.entries.insert(
            name,
            SymbolEntry {
                slot,
                type_scheme: OnceLock::new(),
                value: OnceLock::new(),
                referenced: AtomicBool::new(false),
                definition_span: def_span,
            },
        );
    }

    /// Look up the entry at `Dispatch(depth, slot)` in the given scope chain, returning
    /// both the entry AND the name it is actually stored under.
    ///
    /// Returning the name alongside the entry (rather than discarding it, as a prior
    /// version of this function did) lets every caller that resolved `depth`/`slot`
    /// statically for a specific name cross-check that the *runtime* scope chain still
    /// agrees with that assumption. `Dispatch(depth, slot)` is a pure positional
    /// address — if the runtime scope chain's frame count or per-frame entry order ever
    /// diverges from what the static resolver assumed (e.g. a frame-count-affecting bug
    /// elsewhere), this lookup would otherwise silently return a DIFFERENT binding's
    /// value with no error at all. Name verification at the call site turns that silent
    /// corruption into an immediate, precise diagnostic.
    pub fn lookup_in_chain(
        chain: &[Arc<SymbolTable>],
        depth: u32,
        slot: u32,
    ) -> Option<(&str, &SymbolEntry)> {
        let len = chain.len();
        if (depth as usize) >= len {
            return None;
        }
        chain
            .get(len - 1 - depth as usize)
            .and_then(|frame| frame.entries.get_index(slot as usize))
            .map(|(k, e)| (k.as_str(), e))
    }

    pub fn lookup_evidence_in_chain(
        chain: &[Arc<SymbolTable>],
        class_decl_id: u64,
    ) -> Option<&EvidenceEntry> {
        for frame in chain.iter().rev() {
            if let Some(evidence) = frame.evidence.get() {
                if let Some(entry) = evidence.get(&class_decl_id) {
                    return Some(entry);
                }
            }
        }
        None
    }

    pub fn set_evidence(&self, evidence: HashMap<u64, EvidenceEntry>) -> bool {
        self.evidence.set(evidence).is_ok()
    }

    pub fn collect_caller_evidence_from_chain(
        chain: &[Arc<SymbolTable>],
    ) -> Option<HashMap<u64, EvidenceEntry>> {
        let mut collected = HashMap::new();
        for frame in chain.iter().rev() {
            if let Some(evidence) = frame.evidence.get() {
                for (&k, v) in evidence {
                    collected.entry(k).or_insert_with(|| EvidenceEntry {
                        handler_frame: Arc::clone(&v.handler_frame),
                        method_slots: v.method_slots.clone(),
                    });
                }
            }
        }
        if collected.is_empty() {
            None
        } else {
            Some(collected)
        }
    }

    pub fn set_type_scheme(&self, slot: u32, scheme: Arc<Value>) -> bool {
        self.entries
            .get_index(slot as usize)
            .map_or(false, |(_, entry)| entry.type_scheme.set(scheme).is_ok())
    }

    pub fn set_value(&self, slot: u32, thunk: Arc<Thunk>) -> bool {
        self.entries
            .get_index(slot as usize)
            .map_or(false, |(_, entry)| entry.value.set(thunk).is_ok())
    }

    pub fn mark_referenced(&self, slot: u32) {
        if let Some((_, entry)) = self.entries.get_index(slot as usize) {
            entry.referenced.store(true, Ordering::Relaxed);
        }
    }

    pub fn is_referenced(&self, slot: u32) -> bool {
        self.entries
            .get_index(slot as usize)
            .map_or(false, |(_, e)| e.referenced.load(Ordering::Relaxed))
    }

    pub fn entries(&self) -> &IndexMap<String, SymbolEntry> {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn definition_span(&self, slot: u32) -> Option<&Span> {
        self.entries
            .get_index(slot as usize)
            .map(|(_, e)| &e.definition_span)
    }

    pub fn entry_name(&self, slot: u32) -> Option<&str> {
        self.entries
            .get_index(slot as usize)
            .map(|(name, _)| name.as_str())
    }

    pub fn type_instantiation(&self) -> Option<&HashMap<String, Arc<Value>>> {
        self.type_instantiation.as_ref()
    }

    pub fn evidence(&self) -> Option<&HashMap<u64, EvidenceEntry>> {
        self.evidence.get()
    }

    pub fn extend_chain(
        chain: &ScopeChain,
        named_entries: IndexMap<String, Arc<Thunk>>,
    ) -> ScopeChain {
        let mut entries = IndexMap::new();
        for (slot, (name, thunk)) in named_entries.into_iter().enumerate() {
            let def_span = thunk.definition_span();
            let entry = SymbolEntry {
                slot: slot as u32,
                type_scheme: OnceLock::new(),
                value: OnceLock::new(),
                referenced: AtomicBool::new(false),
                definition_span: def_span,
            };
            entry
                .value
                .set(thunk)
                .expect("fresh OnceLock must accept first write");
            entries.insert(name, entry);
        }
        let new_frame = Arc::new(Self {
            entries,
            evidence: OnceLock::new(),
            type_instantiation: None,
            stage: AtomicU8::new(STAGE_RUNTIME),
        });
        let mut new_chain = chain.clone();
        new_chain.push(new_frame);
        new_chain
    }

    pub fn scope_chain_notes(chain: &[Arc<SymbolTable>], target_depth: Option<u32>) -> Vec<String> {
        let mut notes = Vec::new();
        notes.push(format!("scope chain: {} frames", chain.len()));
        for (depth, frame) in chain.iter().rev().enumerate() {
            let prefix = if target_depth == Some(depth as u32) {
                "--> "
            } else {
                "    "
            };
            let names: Vec<&str> = frame.entries.keys().map(|s| s.as_str()).take(5).collect();
            notes.push(format!(
                "{}scope[{}]: {} entries [{}{}]",
                prefix,
                depth,
                frame.len(),
                names.join(", "),
                if frame.len() > 5 { " ..." } else { "" },
            ));
        }
        notes
    }

    /// Find `name` in `chain`, in stage-priority order: for each stage in `stages` (in
    /// order), scan the chain innermost-to-outermost, skipping `skip` matches within
    /// that stage's pass before returning a match. Only considers `stages[1]` etc. if
    /// `stages[0]` matches nowhere in the chain.
    ///
    /// This is the single correct implementation of "a name may legitimately be bound
    /// at more than one stage; the caller's stage priority must override mere lexical
    /// depth" — every name-based chain lookup in this codebase converges on this
    /// function (directly, or via `get_in_chain_staged` below). Do not reimplement this
    /// search elsewhere: a prior second implementation (a flat, stage-unaware scan) was
    /// exactly how `@Callable`-style annotations could silently resolve to a
    /// same-named binding at the wrong stage, because closer-in-depth won regardless of
    /// stage. Every caller must state its own `stages` priority explicitly — there is no
    /// universally-correct default (type expressions want `[Type, Runtime]`; ordinary
    /// runtime value references want `[Runtime]` only; other needs may differ again).
    pub fn find_in_chain_staged<'a>(
        chain: &'a [Arc<SymbolTable>],
        name: &str,
        stages: &[Stage],
        skip: u32,
    ) -> Option<(u32, &'a Arc<SymbolTable>, &'a SymbolEntry)> {
        for &want in stages {
            let mut seen = 0u32;
            for (depth, frame) in chain.iter().rev().enumerate() {
                if frame.stage() != want {
                    continue;
                }
                if let Some(entry) = frame.entries.get(name) {
                    if seen < skip {
                        seen += 1;
                        continue;
                    }
                    return Some((depth as u32, frame, entry));
                }
            }
        }
        None
    }

    /// Stage-prioritized `name` lookup that materializes the match to a thunk, with the
    /// same "entry exists but its value has not been written" diagnostic as a flat scan
    /// would need. Built on `find_in_chain_staged` — see its doc comment for the stage
    /// rule. Use this whenever the caller only needs the bound value (not depth/slot/
    /// referenced-tracking bookkeeping); `find_in_chain_staged` directly otherwise.
    pub fn get_in_chain_staged(
        chain: &[Arc<SymbolTable>],
        name: &str,
        stages: &[Stage],
    ) -> crate::error::DiagnosticResult<Option<Arc<Thunk>>> {
        match Self::find_in_chain_staged(chain, name, stages, 0) {
            Some((_, _, entry)) => match entry.value.get() {
                Some(thunk) => crate::error::DiagnosticResult::ok(Some(thunk.clone())),
                None => crate::error::DiagnosticResult::err(crate::error::Diagnostic::error(
                    "scope",
                    format!(
                        "entry '{}' exists at slot {} but its value has not been written",
                        name, entry.slot
                    ),
                    entry.definition_span.clone(),
                )),
            },
            None => crate::error::DiagnosticResult::ok(None),
        }
    }
}

impl PartialEq for SymbolTable {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for SymbolTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SymbolTable({} entries)", self.entries.len(),)
    }
}

impl std::fmt::Debug for SymbolEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SymbolEntry(slot: {}, type: {}, value: {})",
            self.slot,
            self.type_scheme.get().is_some(),
            self.value.get().is_some()
        )
    }
}

impl std::fmt::Debug for EvidenceEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EvidenceEntry(methods: {:?})", self.method_slots)
    }
}
