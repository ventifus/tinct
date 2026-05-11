# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## LSP Improvements

### lsp-caps-and-on-demand: LSP caps assumption + on-demand file loading

- [x] [Major] Skip caps validation in LSP mode — pre-seed eval env with stub cap values
- [x] [Major] On-demand hover for unopened documents — load from disk if not in document map
- [x] [Major] On-demand goto-definition for unopened documents
- [x] [Minor] Extract shared `load_doc_from_uri` helper in document.rs
- [x] [Minor] Add 3 LSP corpus tests for unopened document hover/goto/caps

---

## Doc Verification (completed)

---

## Planned Features (from doc spec)

---

## Research (requires /rnd before implementing)

- Mappable constraint checking — requires HKT design (`f :: * → *`); write design note in `doc/whatif/` first
- http3-session builtin — requires persistent async handle storage design first
- Persistent async handle storage — design required before streaming session builtins

