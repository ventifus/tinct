use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use indexmap::IndexMap;

use crate::ast::Span;
use crate::value::{Thunk, Value};

pub struct EvidenceEntry {
    pub handler_frame: Arc<SymbolTable>,
    pub method_slots: Vec<u32>,
}

pub struct SymbolEntry {
    pub slot: u32,
    pub type_scheme: OnceLock<Arc<Value>>,
    pub value: OnceLock<Arc<Thunk>>,
    pub referenced: AtomicBool,
    pub definition_span: Option<Span>,
}

pub struct SymbolTable {
    entries: IndexMap<String, SymbolEntry>,
    parent: Option<Arc<SymbolTable>>,
    evidence: OnceLock<HashMap<u64, EvidenceEntry>>,
    type_instantiation: Option<HashMap<String, Arc<Value>>>,
}

impl SymbolTable {
    pub fn new(parent: Option<Arc<SymbolTable>>) -> Self {
        Self {
            entries: IndexMap::new(),
            parent,
            evidence: OnceLock::new(),
            type_instantiation: None,
        }
    }

    pub fn new_call_frame(
        parent: Arc<SymbolTable>,
        evidence: Option<HashMap<u64, EvidenceEntry>>,
        type_inst: Option<HashMap<String, Arc<Value>>>,
    ) -> Self {
        let ev = OnceLock::new();
        if let Some(e) = evidence {
            ev.set(e).expect("fresh OnceLock must accept first write");
        }
        Self {
            entries: IndexMap::new(),
            parent: Some(parent),
            evidence: ev,
            type_instantiation: type_inst,
        }
    }

    pub fn empty() -> Arc<Self> {
        Arc::new(Self::new(None))
    }

    pub fn insert(&mut self, name: String, slot: u32, def_span: Option<Span>) {
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

    pub fn lookup(&self, depth: u32, slot: u32) -> Option<&SymbolEntry> {
        let mut current = self;
        for _ in 0..depth {
            current = current.parent.as_ref()?;
        }
        current.entries.get_index(slot as usize).map(|(_, e)| e)
    }

    pub fn lookup_by_name(&self, name: &str) -> Option<(u32, &SymbolEntry)> {
        let mut depth = 0u32;
        let mut current = self;
        loop {
            if let Some(entry) = current.entries.get(name) {
                return Some((depth, entry));
            }
            match &current.parent {
                Some(p) => {
                    current = p;
                    depth += 1;
                }
                None => return None,
            }
        }
    }

    pub fn lookup_evidence(&self, class_decl_id: u64) -> Option<&EvidenceEntry> {
        let mut current = self;
        loop {
            if let Some(evidence) = current.evidence.get() {
                if let Some(entry) = evidence.get(&class_decl_id) {
                    return Some(entry);
                }
            }
            match &current.parent {
                Some(p) => current = p,
                None => return None,
            }
        }
    }

    pub fn set_evidence(&self, evidence: HashMap<u64, EvidenceEntry>) -> bool {
        self.evidence.set(evidence).is_ok()
    }

    pub fn collect_caller_evidence(&self) -> Option<HashMap<u64, EvidenceEntry>> {
        let mut collected = HashMap::new();
        let mut current = self;
        loop {
            if let Some(evidence) = current.evidence.get() {
                for (&k, v) in evidence {
                    collected.entry(k).or_insert_with(|| EvidenceEntry {
                        handler_frame: Arc::clone(&v.handler_frame),
                        method_slots: v.method_slots.clone(),
                    });
                }
            }
            match &current.parent {
                Some(p) => current = p,
                None => break,
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

    pub fn parent(&self) -> Option<&Arc<SymbolTable>> {
        self.parent.as_ref()
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
            .and_then(|(_, e)| e.definition_span.as_ref())
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

    pub fn extend(self: &Arc<Self>, named_entries: IndexMap<String, Arc<Thunk>>) -> Arc<Self> {
        let mut entries = IndexMap::new();
        for (slot, (name, thunk)) in named_entries.into_iter().enumerate() {
            let entry = SymbolEntry {
                slot: slot as u32,
                type_scheme: OnceLock::new(),
                value: OnceLock::new(),
                referenced: AtomicBool::new(false),
                definition_span: None,
            };
            entry
                .value
                .set(thunk)
                .expect("fresh OnceLock must accept first write");
            entries.insert(name, entry);
        }
        Arc::new(Self {
            entries,
            parent: Some(Arc::clone(self)),
            evidence: OnceLock::new(),
            type_instantiation: None,
        })
    }

    pub fn get(&self, name: &str) -> Option<Arc<Thunk>> {
        let mut current = self;
        loop {
            if let Some(entry) = current.entries.get(name) {
                return entry.value.get().cloned();
            }
            match &current.parent {
                Some(p) => current = p,
                None => return None,
            }
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
        write!(
            f,
            "SymbolTable({} entries, parent: {})",
            self.entries.len(),
            self.parent.is_some()
        )
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
