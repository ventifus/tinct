---
description: Review a completed whatif — sprint coverage, implementation verification, doc consistency. Ensures sprints are complete and properly scoped, implementation matches spec, feature doc is standalone, main docs are atemporal and complete.
argument-hint: <whatif-name>
allowed-tools: Agent, Read, Write, Edit, Glob, Grep, mcp__mempalace-tinct__*, mcp__tracker__*
model: opus
---

Full consistency review for a completed whatif. Four check areas run in sequence, all findings collected, then reported and fixed together:

1. **Sprint coverage** — are all sprints for this whatif complete? If any are incomplete, are they properly scoped for `/sprint`?
2. **Implementation verification** — does the source code match what the whatif specified? Stubs, deferred code, de-scoped features, and divergences are flagged.
3. **Doc consistency** — feature doc standalone, main docs cover the content, no temporal language anywhere.
4. **Fix pass** — apply approved fixes; reschedule divergences as tracker items.

## Doc Layer Model

| Layer | Location | Role | Temporal language? |
|---|---|---|---|
| Design history | `doc/whatif/` | Primary historical artifact — read-only in this skill | Allowed |
| Deep-dive spec | `doc/feature/` | Optional technical spec; standalone, no whatif references | No |
| Authoritative reference | `doc/*.md` | Complete, atemporal; reads as if always this way | Never |

Content flows forward only: whatif → (feature doc) → main doc. Never backward.

## Arguments

`<whatif-name>` — bare name (`constraint-annotations`), relative path (`doc/whatif/completed/constraint-annotations.md`), or any path under `doc/whatif/`.

## Workflow

### Step 1: Locate and Read the Whatif

1. Resolve the argument to a full path (search `completed/`, `doc/whatif/`, `abandoned/` for bare names)
2. Read the whatif in full
3. Extract and record:
   - **State** (`State:` header)
   - **Supersession headers** — check for these optional fields immediately after `State:`:
     - `**Replaces:** [name.md](...)` — this whatif supersedes an earlier one; extract the target path(s)
     - `**Superseded by:** [name.md](...)` — this whatif was superseded; extract the successor path
     - `**Resolved by:** sprint-slug` — superseded by an implementation sprint with no successor whatif
   - **Sprint slugs** — look for any explicit sprint names in Phased Adoption, Implementation Sprints, or sprint-slug style references (`sprint-name` in backticks, or `### slug:` patterns)
   - **Key identifiers** — every concrete thing the whatif says should exist: type names (`Type::X`, `Expr::X`, `Value::X`), builtin names (`builtin-foo`, `$foo`), stdlib function names, struct fields (`field_name`), error codes (`E0NN`), syntax forms (`[keyword ...]`), CLI flags (`--flag-name`), config keys
   - **Behavioral claims** — things the whatif says should happen: "when X, Y is returned", "Z is rejected with error", "A unifies with B"
   - **Target chapters** — any explicit `doc/*.md` references in the whatif
4. Report: whatif name, state, any supersession links, extracted sprint slugs, and key identifier list

**State guards:**
- `Proposal`: warn — "still a proposal; main docs should NOT contain this content yet." Invert coverage checks (gaps are correct; presence is the warning).
- `Superseded`: identify the superseding whatif. Checks focus on ensuring the old design is NOT in main docs.
- `Completed` or `Accepted`: proceed normally.

---

### Step 2: Supersession Link Verification

Before checking sprints, verify that any supersession headers found in Step 1 are correctly wired in both directions.

**If this whatif has `**Replaces:** [X](path)`:**
1. Read `path` and check that it contains `**Superseded by:**` pointing back to the current whatif
2. If the back-reference is missing: record `LINK-BROKEN: <path> missing **Superseded by:** pointing to <current>`
3. If the back-reference points to the wrong file: record `LINK-WRONG: <path> **Superseded by:** points to <X> not <current>`
4. Check that the referenced file's `State:` says `Superseded` — if it still says `Accepted` or `Proposal`, record `STATE-STALE: <path> still has State: <current-value>`

**If this whatif has `**Superseded by:** [X](path)`:**
1. Read `path` and check that it contains `**Replaces:**` that includes the current whatif's filename
2. If the forward-reference is missing: record `LINK-BROKEN: <path> missing **Replaces:** for <current>`

**If this whatif has `**Resolved by:** sprint-slug`:**
1. Check the tracker: `mcp__tracker__sprint_list(state="done")` — filter for a sprint whose name matches the slug.
2. If not found: record `LINK-BROKEN: sprint-slug not found in tracker`

**If the whatif's `State:` says `Superseded` but has no `**Superseded by:**` or `**Resolved by:**` header:**
Record `LINK-MISSING: State is Superseded but no successor reference found`.

---

### Step 3: Sprint Coverage Check

Determine whether every sprint associated with this whatif is complete, and whether any incomplete sprint is ready for `/sprint`.

#### 3a: Find All Sprints

Sprints are linked to this whatif via `**Whatif:** \`<name>\`` in their header. This is the canonical lookup — use it first, then fall back to name-based search for sprints that predate the convention.

**Tracker lookup:**
1. In the **tracker (done sprints)**: call `mcp__tracker__sprint_list(state="done")` and filter sprints whose name matches known sprint slugs from the whatif. Mark as `DONE`.
2. In the **tracker (backlog sprints)**: call `mcp__tracker__sprint_list(state="backlog")` and filter similarly. For each, call `mcp__tracker__sprint_get` to check item completion: `COMPLETE` (all items done), `IN PROGRESS` (mixed), `NOT STARTED` (all backlog).
3. **Implied sprints** — if the whatif names a sprint slug explicitly (in backticks or as `### slug:`) that does not appear in the tracker, mark as `MISSING`.

#### 3b: Assess Incomplete Sprints

For each sprint found in the tracker backlog:

Call `mcp__tracker__sprint_get(sprint_id)` and check sprint readiness for `/sprint`. A sprint is ready when ALL of the following hold:
- No items with `type="decision"` or `type="research"` in `backlog` state (unresolved design questions)
- No hedged item titles: "consider", "optionally", "possibly", "if needed", "might", "could"
- Item titles reference at least one source file (e.g., `` `src/file.rs` `` or `` `src/file.rs:line` ``)
- At least one item is explicitly a test task
- All sprint `dependencies` (from `sprint_get.dependencies`) have state `"done"` in the tracker

If a sprint is NOT ready, list each gap as `SPRINT-GAP: <sprint-slug> — <gap description>`.

**If `MISSING`:**

This sprint was never created. Record as `SPRINT-MISSING: <sprint-slug>`. The whatif described work that has no tracking entry — this needs a new tracker sprint.

#### 3c: Check Sprint-to-Whatif Alignment

For each incomplete backlog sprint, read its items via `sprint_get`. Verify the items reflect the whatif's intent:
- Do the items cover everything the whatif's corresponding phase described?
- Are there items that don't correspond to anything in the whatif (scope creep without a design)?
- Are there things in the whatif phase that have no corresponding item (scope undercount)?

Record each misalignment as `SPRINT-ALIGN: <sprint-slug> — <description>`.

---

### Step 4: Implementation Verification

For each whatif that is fully done (all sprints in the tracker with state `done`), or for the completed portion of a partially-done whatif, verify the source code matches the specification.

#### 4a: Verify Key Identifiers Exist

For each identifier extracted in Step 1, search the source code:

| Identifier type | Search target | Verdict |
|---|---|---|
| `Type::X`, `Expr::X`, `Value::X`, `ErrorKind::X` | `src/*.rs` — grep for the variant name | PRESENT / MISSING |
| Builtin `foo` | `src/builtins*.rs` — grep for `"foo"` in string literals and match arms | PRESENT / MISSING |
| Stdlib function `foo-bar` | `stdlib/prelude.llt` or `stdlib/*.llt` | PRESENT / MISSING |
| Struct field `field_name` | Relevant `src/*.rs` struct definitions | PRESENT / MISSING |
| Error code `E0NN` | `src/error.rs` or error enum definitions | PRESENT / MISSING |
| CLI flag `--flag-name` | `src/main.rs` — clap argument definitions | PRESENT / MISSING |

For each PRESENT identifier, briefly read the surrounding context to verify it matches the whatif's description (not just a name match on something unrelated).

#### 4b: Check for Stubs and Deferred Code

Search the code paths associated with this whatif's identifiers for patterns indicating incomplete implementation:

- `unimplemented!()`, `todo!()`, `unreachable!()` in a path that should be reachable
- `// TODO`, `// FIXME`, `// STUB`, `// HACK`, `// DEFERRED` comments in related functions
- `#[allow(dead_code)]` on identifiers the whatif says should be active
- Return values that are obviously placeholder: `Ok(())` where a real value is expected, `Type::Unknown` where a specific type was promised, `None` where `Some(...)` was specified
- Function bodies consisting only of `todo!()` or a comment explaining what's missing
- Comments like "deferred to future sprint", "not yet wired", "stub for X"

For each hit: record `STUB: <file>:<line> — <description>`.

#### 4c: Check for De-scoped or Cancelled Features

Search for evidence that something the whatif specified was intentionally removed or reduced in scope without being reflected in the whatif itself:

- Tracker sprint context notes that include "CANCELLED", "de-scoped", "removed", "not implemented", "decided against"
- Git commit messages (if accessible) mentioning removal of features in this area
- Comments in source code saying "was going to do X but..." or "removed X because..."

For each hit: record `DE-SCOPED: <location> — <description>`. These need to either:
1. Be reflected in the whatif (update whatif State to Superseded-in-part), OR
2. Be rescheduled in the tracker as an unassigned item if the de-scope was unintentional

#### 4d: Spot-Check Behavioral Claims

For the most important behavioral claims extracted in Step 1 (pick 3-5 if there are many), verify the implementation:

- Read the relevant source function or code path
- Confirm the behavior matches the specification
- Check error messages match error codes specified in the whatif
- Check type signatures match what the whatif said they would be

Record each divergence as `DIVERGENCE: <file>:<line> — expected: <whatif says> / found: <actual code>`.

---

### Step 5: Doc Consistency Check

#### 5a: Feature Doc

Check whether `doc/feature/<name>.md` exists.

**If it exists:**
1. Search for any string matching `doc/whatif`, `whatif/`, or bare whatif filename — each is a `VIOLATION: feature doc references whatif`
2. Search for temporal/hedging language (see phrase list in §4c)
3. Compare content against the whatif — record `GAP` for anything in the whatif that's absent from the feature doc

**If missing:** note it; not required unless main doc gaps are too large for the chapter format.

#### 5b: Main Doc Coverage

For each relevant `doc/*.md` chapter (determined from whatif's target chapters + the routing table below):

| Feature touches... | Primary chapter |
|---|---|
| Syntax, grammar, parsing | `doc/02-syntax.md` |
| Data model, types, values | `doc/03-data-model.md` |
| Functions, closures | `doc/04-functions.md` |
| Type annotations, `@` syntax | `doc/05-type-annotations.md` |
| Type inference, HM, constraints | `doc/06-type-inference.md` |
| BAS, row poly, gradual typing | `doc/07-type-extensions.md` |
| Evaluation, thunks, laziness | `doc/08-evaluation.md` |
| Documents, pipeline, `---` | `doc/09-documents.md` |
| Errors, error codes, `try` | `doc/10-errors.md` |
| Stdlib functions | `doc/11-stdlib.md` |
| Builtins, DirCap, NetCap | `doc/11a-builtins.md` |
| CLI flags, tooling, LSP | `doc/12-tooling.md` |
| Patterns, match, narrowing | `doc/14-patterns.md` |
| Architecture, internals | `doc/16-architecture.md` |

For each chapter, search for the key identifiers from Step 1. Classify each as OK / STALE / GAP.

#### 5c: Temporal Language

Scan all relevant main doc chapters and the feature doc for:

- **Temporal**: "previously", "used to", "now", "as of", "since [sprint]", "was changed", "replaced by", "no longer"
- **Forward-looking**: "will be", "will add", "will support", "planned", "future", "not yet", "eventually", "coming soon", "TODO", "TBD", "deferred"
- **Hedging**: "currently", "at this time", "for now", "may", "might", "optionally"
- **Implementation-tracking**: "backward compat", "during migration", "see TODO.md", "tracked in TODO", "Phase [0-9]"

Ignore hits inside fenced code blocks quoting actual source code comments, citations, or ARCHIVED sections.

Record each as `TEMPORAL: <file>:<line> — "<phrase>"`.

---

### Step 6: Report

Present all findings in a single structured report:

```
## Whatif Review: <name>
**State:** <state>
**Summary:** <one sentence>

### Supersession Links
LINK-OK: <Replaces/Superseded-by pair verified>
LINK-BROKEN: <file> — <what's missing>
LINK-WRONG: <file> — <what it points to vs what it should>
STATE-STALE: <file> — State: <value> should be Superseded
LINK-MISSING: State is Superseded but no successor reference found

### Sprint Coverage
<sprint-slug> — DONE | COMPLETE | IN PROGRESS | NOT STARTED | MISSING
LEGACY-LINK: <slug> — associated via name match but missing **Whatif:** field; add it
SPRINT-GAP: <slug> — <gap: missing file reference, hedged language, etc.>
SPRINT-MISSING: <slug> — <never created>
SPRINT-ALIGN: <slug> — <misalignment between sprint tasks and whatif scope>

### Implementation
PRESENT: <identifier>
MISSING: <identifier> — <should exist per whatif but not found>
STUB: <file>:<line> — <description>
DE-SCOPED: <location> — <description>
DIVERGENCE: <file>:<line> — expected: <X> / found: <Y>

### Feature Doc: doc/feature/<name>.md
[EXISTS | MISSING]
VIOLATION: <file>:<line> — whatif reference: "<text>"
TEMPORAL: <file>:<line> — "<phrase>"
GAP: <section or concept>

### Main Doc Coverage
<chapter>:
  <identifier> — OK | STALE | GAP

TEMPORAL: <file>:<line> — "<phrase>"

### Priority Action List
1. RESCHEDULE: <divergences and missing identifiers that need new tracker items>
2. FIX SPRINT: <sprint readiness gaps>
3. FIX CODE: <stubs to remove, divergences to address>
4. FIX DOC: <content gaps, stale content, temporal language>
```

After presenting, ask: "Which of these would you like me to address first? I can:
- Create tracker items for missing/diverged work
- Tighten up incomplete sprint definitions in the tracker
- Fix doc gaps and temporal language"

---

### Step 7: Apply Fixes (interactive, user-approved)

**Rescheduling (highest priority — missing or diverged implementation)**

For each `MISSING`, `DE-SCOPED`, or `DIVERGENCE` finding:
- Propose a concrete tracker item that tracks the missing work — show the user before creating
- Frame it as a concrete implementation task (not "investigate" or "consider")
- Create as an unassigned item (`mcp__tracker__item_create(type="task"/"bug", title="...", source_dialog="review-whatif [name]: [MISSING/DIVERGENCE/DE-SCOPED]", source_file="doc/whatif/[path]")`) so grooming can assign it; or assign it to an existing related sprint if obvious
- Show the user the proposed item before creating

For each `STUB`:
- If a backlog sprint already covers this area, add the item to it (`item_create(type="task", title="Remove stub: [description]", sprint_id=..., source_dialog="review-whatif [name]: STUB at [file:line]", source_file="doc/whatif/[path]")`)
- Otherwise create an unassigned item — grooming will assign it

**Sprint readiness fixes (for SPRINT-GAP findings)**

For each gap in an existing backlog sprint:
- Add file references to items that lack them: `mcp__tracker__item_update(item_id, title="...(`src/file.rs`)")`
- Rewrite hedged item titles as concrete tasks: `item_update(item_id, title="...")`
- Add a test item if none exists: `item_create(type="task", title="Tests: ...", sprint_id=..., source_dialog="review-whatif [name]: sprint [slug] readiness gap — missing test task")`
- Remove unresolved decision/research items by routing them through `/rnd` first
- Show all proposed changes to the user before applying

**Doc fixes (last)**

In priority order:
1. Content gaps in main docs — draft section, show user, apply on approval
2. Stale content — draft correction, show user, apply on approval  
3. Temporal language — batch phrase-level edits, show summary, apply on approval
4. Feature doc violations — remove whatif references or inline the content, show user first

**Never:**
- Modify `doc/whatif/` files — permanently read-only
- Apply any edit without showing the user the proposed change first
- Invent implementation content — all doc additions must trace back to the whatif or source code
- Write implementation code — rescheduling means tracker items, not source changes

---

### Step 8: Update Tracker

After all approved fixes:
- If new sprints were created for `RESCHEDULE` or `FIX SPRINT` findings, confirm they exist in the tracker as backlog sprints with context notes explaining the finding
- If a sprint was found complete (all tracker items done) but somehow still showing as backlog, call `mcp__tracker__sprint_complete(sprint_id)` to finalize it
- If the whatif's review sprint itself is now satisfied, mark it done in the tracker

## Key Principles

- **Whatifs are read-only.** They are the primary historical artifact. This skill never edits them.
- **Sprint coverage before doc.** Incomplete sprints are the highest-severity finding — they mean work is untracked, not just undocumented.
- **Implementation before docs.** A `MISSING` or `DIVERGENCE` finding outranks any doc gap.
- **Reschedule, don't delete.** De-scoped or cancelled features need a tracker item explaining the decision, or the whatif needs a `Superseded-in-part` note. Neither is silently dropped.
- **Feature docs are standalone.** No `doc/whatif/` references — they should be fully self-contained.
- **Main docs are atemporal.** Any phrase that reveals when something was added is a violation.
- **One whatif per invocation.** Do not try to review multiple whatifs in one run.
